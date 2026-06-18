//! RPC method handlers. One function per protocol method.

use crate::cursor as motion;
use crate::error::RpcError;
use crate::grep;
use crate::picker as picker_state;
use crate::state::MOTION_HISTORY_CAP;
use crate::state::{
    BlameCache, Buffer, EditKindTag, LineEnding, NavEntry, SearchEntry, ServerState, SharedState,
    Viewport,
};
use crate::surround;
use crate::wrap;
use aether_protocol::buffer::{
    BufferCloseParams, BufferClosed, BufferClosedParams, BufferCopyParams, BufferCopyResult,
    BufferCutResult, BufferOpenParams, BufferOpenResult, BufferReloadParams, BufferReloadResult,
    BufferSaveParams, BufferSaveResult, BufferState, BufferStateParams, CopyScope,
};
use aether_protocol::cursor::{
    CursorBufferOnlyParams, CursorMoveParams, CursorSelectLineParams, CursorSetParams, CursorState,
    CursorSwapAnchorParams, CursorUndoParams, CursorUndoResult, Direction, Granularity,
    GrepPosition, Motion, VerticalDirection,
};
use aether_protocol::directory::{
    DirectoryCreateParams, DirectoryCreateResult, DirectoryEntry, DirectoryListParams,
    DirectoryListResult,
};
use aether_protocol::envelope::{JsonRpc, Notification, NotificationMethod};
use aether_protocol::error::ErrorCode;
use aether_protocol::git::{
    ApplyHunkStatus, GitApplyHunkParams, GitApplyHunkResult, GitBlameLineParams,
    GitBlameLineResult, GitBufferStatus, GitChangeCounts, GitCommitInfoParams, GitCommitInfoResult,
    GitNavigateHunkParams, GitNavigateHunkResult, GitSetDiffViewParams, HunkAction, HunkDirection,
};
use aether_protocol::input::{
    BufferOnlyParams, CountedEditParams, EditResult, InputMoveLinesParams, InputOpenLineParams,
    InputSurroundParams, InputTextParams, InputUnsurroundParams, LineSide, SurroundTarget,
    UndoResult,
};
use aether_protocol::lsp::{
    DiagnosticCounts, DiagnosticDirection, FormatStatus, LspBufferParams, LspDiagnosticsChanged,
    LspDiagnosticsChangedParams, LspFormatResult, LspGotoDefinitionResult, LspHoverResult,
    LspLocation, LspNavigateDiagnosticParams, LspNavigateDiagnosticResult, LspRestartServerParams,
    LspServerStatusParams, LspServerStatusResult, LspStatus,
};
use aether_protocol::nav::{
    NavGotoParams, NavRecordParams, NavRecordResult, NavStepParams, NavStepResult,
};
use aether_protocol::path::{PathDeleteParams, PathDeleteResult};
use aether_protocol::picker::{
    BufferDirtyState, CaseMode, MatchOptions, PickerGrepFileJumpParams, PickerGrepNavigateParams,
    PickerGrepNavigateTarget, PickerHideParams, PickerItem, PickerKind, PickerQueryParams,
    PickerSelectParams, PickerSelectResult, PickerUpdate, PickerUpdateParams, PickerViewParams,
    PickerViewResult,
};
use aether_protocol::project::{
    ProjectActivateParams, ProjectActivateResult, ProjectAddRootParams, ProjectCreateParams,
    ProjectDeleteParams, ProjectInfo, ProjectListParams, ProjectListResult,
    ProjectRemoveRootParams, ProjectRemoveRootResult, ProjectRenameParams, ProjectRenamed,
    ProjectRenamedParams, ProjectSummary,
};
use aether_protocol::search::{
    SearchClearParams, SearchMatchRange, SearchNavParams, SearchNavResult, SearchSetParams,
    SearchSetResult, SearchStateChanged, SearchSummary,
};
use aether_protocol::settings::{AppSettings, SettingsChanged, SettingsGetParams};
use aether_protocol::viewport::{
    BufferStatusSnapshot, DiagnosticSpan, DiffMarker, DiffStage, LogicalLineRange,
    LogicalLineRender, ScrollPosition, ViewportLinesChanged, ViewportLinesChangedParams,
    ViewportResizeParams, ViewportScrollParams, ViewportSetWrapParams, ViewportSubscribeParams,
    ViewportSubscribeResult, ViewportUnsubscribeParams, ViewportWindowResult, VirtualRow,
    VirtualRowKind, Window,
};
use aether_protocol::LogicalPosition;
use aether_protocol::{BufferId, ClientId, Revision};
use std::borrow::Cow;
use std::collections::HashMap;
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

// ---- project/* --------------------------------------------------------------------------------

/// Enumerate the projects configured on disk under `$XDG_CONFIG_HOME/aether/projects/`. The
/// caller uses this to populate the project picker. Doesn't indicate which project (if any) the
/// caller has active — the client tracks that locally.
pub async fn project_list(
    _state: &SharedState,
    _ctx: &mut ConnectionCtx,
    _params: ProjectListParams,
) -> Result<ProjectListResult, RpcError> {
    let names = crate::config::list_project_names()
        .map_err(|e| RpcError::internal(format!("listing projects: {e}")))?;
    Ok(ProjectListResult {
        projects: names
            .into_iter()
            .map(|name| ProjectSummary { name })
            .collect(),
    })
}

/// Read the global application settings (`$XDG_CONFIG_HOME/aether/settings.toml`). Returns defaults
/// when no settings file exists yet. App-wide, so it ignores the caller's active project.
pub async fn settings_get(
    _state: &SharedState,
    _ctx: &mut ConnectionCtx,
    _params: SettingsGetParams,
) -> Result<AppSettings, RpcError> {
    crate::config::load_app_settings()
        .map_err(|e| RpcError::internal(format!("loading app settings: {e}")))
}

/// Replace the global application settings and persist them. Echoes the stored settings back, so
/// the caller reconciles against exactly what landed on disk, and pushes `settings/changed` to every
/// *other* connected client (settings are app-wide, so this ignores active projects) so the change
/// applies live everywhere rather than only at the next reconnect.
pub async fn settings_set(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: AppSettings,
) -> Result<AppSettings, RpcError> {
    crate::config::write_app_settings(&params)
        .map_err(|e| RpcError::internal(format!("writing app settings: {e}")))?;

    let changed = serde_json::to_value(params).unwrap_or(serde_json::Value::Null);
    let pushes: PendingPushes = {
        let s = state.lock().await;
        s.clients
            .iter()
            .filter(|(id, _)| **id != ctx.client_id)
            .map(|(_, sess)| {
                (
                    sess.outbound.clone(),
                    Notification {
                        jsonrpc: JsonRpc,
                        method: SettingsChanged::NAME.into(),
                        params: changed.clone(),
                    },
                )
            })
            .collect()
    };
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }

    Ok(params)
}

/// Activate a project for this client. Loads the project's config from disk if no client has it
/// active yet (lazy load). If the client already has a different project active, tears down the
/// client's per-buffer state for the prior project before switching. Returns the resolved
/// project info (name + canonical paths) so the client can present buffers relative to those
/// roots.
pub async fn project_activate(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: ProjectActivateParams,
) -> Result<ProjectActivateResult, RpcError> {
    let client_id = ctx.client_id;

    // Cheap path: if the project is already loaded (some other client activated it earlier, or
    // it was pre-registered for tests), skip the disk read.
    let already_loaded = state.lock().await.projects.contains_key(&params.name);

    // Cold path: read the project's config from disk *outside* the state lock — file I/O and
    // canonicalization can be slow on cold caches, and we hold the lock for many concurrent
    // operations.
    let cold_load: Option<(String, Vec<std::path::PathBuf>)> = if already_loaded {
        None
    } else {
        let cfg = match crate::config::load_project(&params.name) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(name = %params.name, error = %e, "project/activate: project not found");
                return Err(RpcError::unknown_project(&params.name));
            }
        };
        let canonical_paths: Vec<std::path::PathBuf> = cfg
            .paths
            .iter()
            .map(|p| crate::config::canonicalize_project_path(p))
            .collect::<Result<_, _>>()
            .map_err(|e| RpcError::invalid_path(format!("canonicalizing project path: {e}")))?;
        Some((cfg.name, canonical_paths))
    };

    let mut s = state.lock().await;

    // If the client had a different project active, tear down its prior per-buffer state.
    let prior = s
        .clients
        .get(&client_id)
        .and_then(|c| c.active_project.clone());
    if let Some(prior_name) = &prior {
        if prior_name != &params.name {
            s.teardown_client_state_for_project(client_id, prior_name);
        }
    }

    // Install the project entry on the cold path. Reuse the existing entry (and its shared
    // `WorkspaceIndex`) on the hot path.
    if let Some((name, canonical_paths)) = cold_load {
        let workspace_index = Arc::new(crate::workspace_index::WorkspaceIndex::new(
            canonical_paths.clone(),
        ));
        s.projects.insert(
            params.name.clone(),
            crate::state::ProjectEntry {
                name,
                paths: canonical_paths.clone(),
                workspace_index,
                mru_buffers: std::collections::VecDeque::new(),
            },
        );
        // Hand the new roots to the watcher so its events flow for this project too. Best-effort
        // — a watcher failure shouldn't refuse activation. `watcher` is `None` only when the
        // server skipped initializing it (failed at startup); we just skip registration in that
        // case.
        if let Some(w) = s.watcher.clone() {
            crate::watcher::watch_project_paths(&w, &canonical_paths);
        }
    }

    let entry_paths: Vec<String> = s
        .projects
        .get(&params.name)
        .map(|p| p.paths.iter().map(|p| p.display().to_string()).collect())
        .unwrap_or_default();
    let server_started_at = s.started_at_unix_ms;

    if let Some(session) = s.clients.get_mut(&client_id) {
        session.active_project = Some(params.name.clone());
    }

    // Most-recently-used buffer in the newly-active project. The MRU lives on `ProjectEntry`
    // (not per-client) so it survives client disconnects — a fresh TUI invocation sees the same
    // top-of-MRU buffer the prior session left there. The client uses this to reattach instead
    // of spawning a fresh scratch on every switch.
    let last_buffer_id = s.projects.get(&params.name).and_then(|p| {
        p.mru_buffers
            .iter()
            .find(|id| s.buffers.contains_key(id))
            .copied()
    });

    tracing::info!(%client_id, project = %params.name, "client activated project");
    drop(s);

    // Composite post-step (docs/protocol-composites.md, C): open the landing buffer — the
    // project's MRU buffer, or a fresh transient scratch on a first visit — in the same
    // round-trip. Mirrors the convention every client implemented by hand.
    let opened = if params.open_last {
        Some(
            buffer_open(
                state,
                ctx,
                BufferOpenParams {
                    buffer_id: last_buffer_id,
                    transient: if last_buffer_id.is_none() {
                        Some(true)
                    } else {
                        None
                    },
                    ..Default::default()
                },
            )
            .await?,
        )
    } else {
        None
    };

    Ok(ProjectActivateResult {
        project: ProjectInfo {
            name: params.name,
            paths: entry_paths,
        },
        last_buffer_id,
        opened,
        server_started_at,
    })
}

/// Validate and normalize a user-supplied project name. Trims surrounding whitespace, then
/// rejects empty names and names containing path separators — the name becomes a `<name>.toml`
/// filename, so a `/`, `\`, `.`, or `..` could escape the projects dir. Shared by
/// `project/create` and `project/rename`.
fn validate_project_name(raw: &str) -> Result<String, RpcError> {
    let name = raw.trim().to_string();
    if name.is_empty() {
        return Err(RpcError::invalid_params("project name must not be empty"));
    }
    if name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        return Err(RpcError::invalid_params(
            "project name must not contain path separators",
        ));
    }
    Ok(name)
}

/// Create a fresh project with no roots. Writes an empty-`paths` TOML to disk, registers the
/// project in memory, and activates it for the calling client. Refuses if a project of that
/// name already exists, or if the name is empty / contains path separators.
pub async fn project_create(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: ProjectCreateParams,
) -> Result<ProjectActivateResult, RpcError> {
    let client_id = ctx.client_id;
    let name = validate_project_name(&params.name)?;
    let exists = crate::config::project_config_exists(&name)
        .map_err(|e| RpcError::internal(format!("checking project config: {e}")))?;
    if exists {
        return Err(RpcError::invalid_params(format!(
            "project {name} already exists"
        )));
    }
    // Write the TOML outside the state lock — file I/O.
    crate::config::write_project_config(&crate::config::ProjectConfig {
        name: name.clone(),
        paths: Vec::new(),
    })
    .map_err(|e| RpcError::internal(format!("writing project config: {e}")))?;

    let mut s = state.lock().await;

    // Tear down the client's prior project state (same flow as project_activate).
    let prior = s
        .clients
        .get(&client_id)
        .and_then(|c| c.active_project.clone());
    if let Some(prior_name) = &prior {
        if prior_name != &name {
            s.teardown_client_state_for_project(client_id, prior_name);
        }
    }

    // Register the empty project. No paths → no workspace_index walk to do; the empty Arc
    // returns an empty file list on access.
    let workspace_index = Arc::new(crate::workspace_index::WorkspaceIndex::new(Vec::new()));
    s.projects.insert(
        name.clone(),
        crate::state::ProjectEntry {
            name: name.clone(),
            paths: Vec::new(),
            workspace_index,
            mru_buffers: std::collections::VecDeque::new(),
        },
    );
    if let Some(session) = s.clients.get_mut(&client_id) {
        session.active_project = Some(name.clone());
    }
    let server_started_at = s.started_at_unix_ms;

    // Another client's open chooser should gain the new project.
    let pushes = refresh_project_pickers(&mut s);
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }

    tracing::info!(%client_id, project = %name, "client created project");
    Ok(ProjectActivateResult {
        project: ProjectInfo {
            name,
            paths: Vec::new(),
        },
        last_buffer_id: None,
        opened: None,
        server_started_at,
    })
}

/// Add a root path to an existing project. Canonicalizes, refuses duplicates, writes the TOML,
/// registers with the watcher, rebuilds the workspace index. Returns the updated project info.
pub async fn project_add_root(
    state: &SharedState,
    _ctx: &mut ConnectionCtx,
    params: ProjectAddRootParams,
) -> Result<ProjectInfo, RpcError> {
    let canonical = crate::config::canonicalize_project_path(std::path::Path::new(&params.path))
        .map_err(|e| RpcError::invalid_path(format!("canonicalizing root: {e}")))?;

    let mut s = state.lock().await;
    let project = s
        .projects
        .get_mut(&params.project)
        .ok_or_else(|| RpcError::unknown_project(&params.project))?;
    if project.paths.iter().any(|p| p == &canonical) {
        return Err(RpcError::invalid_params(format!(
            "{} is already a root of project {}",
            canonical.display(),
            params.project
        )));
    }
    project.paths.push(canonical.clone());
    // Rebuild workspace_index with the new path list. The old Arc remains alive only for any
    // in-flight reader; subsequent picker opens see the fresh one.
    project.workspace_index = Arc::new(crate::workspace_index::WorkspaceIndex::new(
        project.paths.clone(),
    ));
    let updated = crate::config::ProjectConfig {
        name: project.name.clone(),
        paths: project.paths.clone(),
    };
    let entry_paths: Vec<String> = project
        .paths
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    let watcher = s.watcher.clone();
    drop(s);

    // TOML write + watcher registration happen outside the lock.
    crate::config::write_project_config(&updated)
        .map_err(|e| RpcError::internal(format!("writing project config: {e}")))?;
    if let Some(w) = watcher {
        crate::watcher::watch_project_paths(&w, &[canonical]);
    }
    Ok(ProjectInfo {
        name: params.project,
        paths: entry_paths,
    })
}

/// Remove a root path from a project. Closes any file-backed buffers under this root that
/// aren't covered by another remaining root; refuses with `DIRTY_BUFFERS_PREVENT_REMOVE` if any
/// such buffer is dirty. Scratch buffers in the project are unaffected.
pub async fn project_remove_root(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: ProjectRemoveRootParams,
) -> Result<ProjectRemoveRootResult, RpcError> {
    let client_id = ctx.client_id;
    let canonical = crate::config::canonicalize_project_path(std::path::Path::new(&params.path))
        .map_err(|e| RpcError::invalid_path(format!("canonicalizing root: {e}")))?;

    let mut s = state.lock().await;
    let project = s
        .projects
        .get_mut(&params.project)
        .ok_or_else(|| RpcError::unknown_project(&params.project))?;
    if !project.paths.iter().any(|p| p == &canonical) {
        return Err(RpcError::invalid_params(format!(
            "{} is not a root of project {}",
            canonical.display(),
            params.project
        )));
    }
    let remaining_paths: Vec<std::path::PathBuf> = project
        .paths
        .iter()
        .filter(|p| **p != canonical)
        .cloned()
        .collect();
    let project_name = project.name.clone();

    // Find file-backed buffers under the removed root that aren't covered by any remaining
    // root. Scratch buffers (no path) are exempt; they stay alive.
    let under_removed = |buf: &Buffer| -> bool {
        let Some(p) = buf.canonical_path.as_deref() else {
            return false;
        };
        p == canonical || p.starts_with(&canonical)
    };
    let still_covered = |buf: &Buffer| -> bool {
        let Some(p) = buf.canonical_path.as_deref() else {
            return true;
        };
        remaining_paths
            .iter()
            .any(|root| p == root || p.starts_with(root))
    };
    let affected: Vec<BufferId> = s
        .buffers
        .iter()
        .filter(|(id, buf)| {
            s.buffer_projects.get(id).map(|s| s.as_str()) == Some(&project_name)
                && under_removed(buf)
                && !still_covered(buf)
        })
        .map(|(id, _)| *id)
        .collect();
    let dirty: Vec<BufferId> = affected
        .iter()
        .filter(|id| s.buffers.get(id).map(|b| b.dirty).unwrap_or(false))
        .copied()
        .collect();
    if !dirty.is_empty() {
        let mut err = RpcError::new(
            ErrorCode::DIRTY_BUFFERS_PREVENT_REMOVE,
            format!(
                "{} buffer(s) under {} have unsaved changes",
                dirty.len(),
                canonical.display()
            ),
        );
        err.data = Some(serde_json::json!({ "dirty_buffer_ids": dirty }));
        return Err(err);
    }

    // Other clients viewing any of these buffers must be told to switch — capture before teardown.
    let other_clients = clients_viewing_buffers(&s, &affected, client_id);
    // Close the affected buffers (clean ones). Same teardown as buffer/close.
    for &id in &affected {
        s.close_buffer(id);
    }

    // Persist the updated path list. Re-grab the project mutably for the write.
    let project = s
        .projects
        .get_mut(&params.project)
        .expect("project still loaded — we held it above");
    project.paths.retain(|p| *p != canonical);
    project.workspace_index = Arc::new(crate::workspace_index::WorkspaceIndex::new(
        project.paths.clone(),
    ));
    let updated = crate::config::ProjectConfig {
        name: project.name.clone(),
        paths: project.paths.clone(),
    };
    let entry_paths: Vec<String> = project
        .paths
        .iter()
        .map(|p| p.display().to_string())
        .collect();

    // Next buffer for the requesting client: top of project MRU, else any remaining buffer in
    // the project. Mirrors buffer/close.
    let next_buffer_id = next_buffer_for_client(&s, client_id);
    let watcher = s.watcher.clone();
    let mut pushes = refresh_buffer_pickers(&mut s);
    pushes.extend(buffer_closed_pushes(&s, &other_clients));
    drop(s);

    crate::config::write_project_config(&updated)
        .map_err(|e| RpcError::internal(format!("writing project config: {e}")))?;
    if let Some(w) = watcher {
        crate::watcher::unwatch_project_paths(&w, &[canonical]);
    }
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    tracing::info!(
        project = %params.project,
        closed = affected.len(),
        "root removed"
    );
    Ok(ProjectRemoveRootResult {
        project: ProjectInfo {
            name: params.project,
            paths: entry_paths,
        },
        closed_buffer_ids: affected,
        next_buffer_id,
    })
}

/// Rename a project: move its on-disk config, then re-key every in-memory reference to the old
/// name (the project map, buffer→project associations, and clients' active-project pointers).
/// Open buffers keep their ids and paths and nothing is closed, so this is safe regardless of
/// dirty state. Refuses an empty / separator-bearing name or a collision with an existing
/// project; a no-op when the name is unchanged.
pub async fn project_rename(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: ProjectRenameParams,
) -> Result<ProjectInfo, RpcError> {
    let new_name = validate_project_name(&params.new_name)?;
    let old_name = params.project;

    // Confirm the project is loaded *before* touching disk, so a failure here leaves nothing
    // half-applied. Projects are never removed from the map at runtime, so this stays true for
    // the re-key below.
    {
        let s = state.lock().await;
        let entry = s
            .projects
            .get(&old_name)
            .ok_or_else(|| RpcError::unknown_project(&old_name))?;
        if new_name == old_name {
            // No-op rename — return current info without touching disk or state.
            return Ok(ProjectInfo {
                name: old_name,
                paths: entry
                    .paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect(),
            });
        }
    }

    // Refuse clobbering another project's config; `fs::rename` would otherwise overwrite it.
    let exists = crate::config::project_config_exists(&new_name)
        .map_err(|e| RpcError::internal(format!("checking project config: {e}")))?;
    if exists {
        return Err(RpcError::invalid_params(format!(
            "project {new_name} already exists"
        )));
    }

    // Disk first, outside the lock (file I/O). If this fails, in-memory state is untouched.
    crate::config::rename_project_config(&old_name, &new_name)
        .map_err(|e| RpcError::internal(format!("renaming project config: {e}")))?;

    // Re-key every in-memory reference from the old name to the new one. Projects are never
    // removed from the map at runtime, so the entry we confirmed above is still present.
    let mut s = state.lock().await;
    let entry_paths = s
        .rename_project(&old_name, &new_name)
        .ok_or_else(|| RpcError::internal("project vanished during rename"))?;

    // The re-key above already moved every other connected client on this project to the new name
    // server-side; push `project/renamed` so each can update its *local* name (display + reconnect
    // baseline). The initiating client learns the new name from this RPC's result instead.
    let mut pushes: PendingPushes = s
        .clients
        .iter()
        .filter(|(id, sess)| {
            **id != ctx.client_id && sess.active_project.as_deref() == Some(new_name.as_str())
        })
        .map(|(_, sess)| {
            (
                sess.outbound.clone(),
                Notification {
                    jsonrpc: JsonRpc,
                    method: ProjectRenamed::NAME.into(),
                    params: serde_json::to_value(ProjectRenamedParams {
                        old_name: old_name.clone(),
                        new_name: new_name.clone(),
                    })
                    .unwrap_or(serde_json::Value::Null),
                },
            )
        })
        .collect();
    // ...and any open chooser elsewhere should show the new name in its list.
    pushes.extend(refresh_project_pickers(&mut s));
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }

    tracing::info!(old = %old_name, new = %new_name, "project renamed");
    Ok(ProjectInfo {
        name: new_name,
        paths: entry_paths,
    })
}

/// Delete a project: drop its in-memory state (closing its buffers) and remove its on-disk config.
/// Forgets the project *definition* — source files under its roots are untouched. Refuses if the
/// project is active for any client (the caller must switch away first), or if any of its buffers
/// is dirty.
pub async fn project_delete(
    state: &SharedState,
    _ctx: &mut ConnectionCtx,
    params: ProjectDeleteParams,
) -> Result<(), RpcError> {
    let name = params.name;

    let mut s = state.lock().await;

    // Refuse to delete a project anyone is currently in — that's the rug-pull we promised to
    // prevent. The switcher already greys out the caller's own active project; this also covers
    // other connected clients.
    if s.project_active_anywhere(&name) {
        return Err(RpcError::new(
            ErrorCode::ACTIVE_PROJECT_PREVENTS_DELETE,
            format!("project {name} is active — switch to another project before deleting it"),
        ));
    }

    // Refuse if any buffer in the project has unsaved changes (mirrors `project/remove_root`).
    let dirty: Vec<BufferId> = s
        .buffers_in_project(&name)
        .into_iter()
        .filter(|id| s.buffers.get(id).map(|b| b.dirty).unwrap_or(false))
        .collect();
    if !dirty.is_empty() {
        let mut err = RpcError::new(
            ErrorCode::DIRTY_BUFFERS_PREVENT_DELETE,
            format!(
                "{} buffer(s) in project {name} have unsaved changes",
                dirty.len()
            ),
        );
        err.data = Some(serde_json::json!({ "dirty_buffer_ids": dirty }));
        return Err(err);
    }

    let closed = s.delete_project(&name);
    // Intentionally leave the (now-orphaned) project roots in the watcher: dropping a watch is
    // best-effort and a sibling project may share the same root. Stale watches are harmless — the
    // watcher drops events that don't map to a loaded project.
    drop(s);

    crate::config::delete_project_config(&name)
        .map_err(|e| RpcError::internal(format!("deleting project config: {e}")))?;

    // Re-take the lock to refresh any open chooser — only now is the project gone from disk (the
    // candidate list is a disk read), so the dropped project disappears from the list.
    let pushes = {
        let mut s = state.lock().await;
        refresh_project_pickers(&mut s)
    };
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }

    tracing::info!(project = %name, closed = closed.len(), "project deleted");
    Ok(())
}

/// Delete a file or directory by moving it to the OS trash. Validates the path is inside the
/// active project (and isn't a root itself), refuses if it — or, for a directory, anything under
/// it — has unsaved changes, then trashes it and closes the now-orphaned buffers.
pub async fn path_delete(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: PathDeleteParams,
) -> Result<PathDeleteResult, RpcError> {
    let client_id = ctx.client_id;
    let raw = std::path::PathBuf::from(&params.path);
    // Full canonicalization — the target must exist to be deleted.
    let canonical = std::fs::canonicalize(&raw)
        .map_err(|e| RpcError::invalid_path(format!("canonicalizing {}: {e}", raw.display())))?;

    // Validate the boundary and screen for unsaved changes under the lock, before touching disk.
    {
        let s = state.lock().await;
        let project = s.active_project_or_err(client_id)?;
        if !project.contains(&canonical) {
            return Err(RpcError::invalid_path(format!(
                "{} is outside the project's access boundary",
                canonical.display()
            )));
        }
        if project.paths.iter().any(|p| p == &canonical) {
            return Err(RpcError::invalid_params(format!(
                "{} is a project root — remove it from project settings instead",
                canonical.display()
            )));
        }
        let project_name = project.name.clone();
        let dirty: Vec<BufferId> = s
            .buffers_under_path(&project_name, &canonical)
            .into_iter()
            .filter(|id| s.buffers.get(id).map(|b| b.dirty).unwrap_or(false))
            .collect();
        if !dirty.is_empty() {
            let mut err = RpcError::new(
                ErrorCode::DIRTY_BUFFERS_PREVENT_DELETE,
                format!(
                    "{} buffer(s) under {} have unsaved changes",
                    dirty.len(),
                    canonical.display()
                ),
            );
            err.data = Some(serde_json::json!({ "dirty_buffer_ids": dirty }));
            return Err(err);
        }
    }

    // Move to the OS trash (recoverable) — directories go whole. Outside the lock: filesystem I/O.
    trash::delete(&canonical)
        .map_err(|e| RpcError::file_io(format!("trashing {}: {e}", canonical.display())))?;

    // Close the buffers whose backing file just went to the trash, and refresh.
    let mut s = state.lock().await;
    let Some(project_name) = s
        .clients
        .get(&client_id)
        .and_then(|c| c.active_project.clone())
    else {
        // Client deactivated mid-call — the trash already happened; nothing left to tear down.
        return Ok(PathDeleteResult {
            closed_buffer_ids: Vec::new(),
            next_buffer_id: None,
        });
    };
    let closed = s.buffers_under_path(&project_name, &canonical);
    // Other clients viewing any of these buffers must be told to switch — capture before teardown.
    let other_clients = clients_viewing_buffers(&s, &closed, client_id);
    for &id in &closed {
        s.close_buffer(id);
    }
    // Drop the Files-picker cache so a re-view re-walks without the deleted path. The watcher will
    // also notice the removal, but this keeps the client's immediate refresh consistent.
    if let Some(p) = s.projects.get(&project_name) {
        p.workspace_index.invalidate();
    }
    let next_buffer_id = next_buffer_for_client(&s, client_id);
    let mut pushes = refresh_buffer_pickers(&mut s);
    pushes.extend(buffer_closed_pushes(&s, &other_clients));
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    tracing::info!(path = %canonical.display(), closed = closed.len(), "path trashed");
    Ok(PathDeleteResult {
        closed_buffer_ids: closed,
        next_buffer_id,
    })
}

// ---- buffer/open --------------------------------------------------------------------------------

/// Pick the cursor to return from `buffer/open`. When `clamped_jump` is set, build a fresh
/// point-cursor at that position and persist it into `s.cursors` (overriding any prior state for
/// this `(client, buffer)`). Otherwise return the previously-persisted cursor or default.
fn resolve_open_cursor(
    s: &mut ServerState,
    client_id: Option<ClientId>,
    buffer_id: BufferId,
    clamped_jump: Option<LogicalPosition>,
) -> CursorState {
    if let Some(clamped) = clamped_jump {
        let new = CursorState {
            position: clamped,
            anchor: clamped,
            match_bracket: None,
            grep_position: None,
        };
        if let Some(c) = client_id {
            s.cursors.insert((c, buffer_id), new);
        }
        new
    } else {
        client_id
            .and_then(|c| s.cursors.get(&(c, buffer_id)).copied())
            .unwrap_or_default()
    }
}

pub async fn buffer_open(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: BufferOpenParams,
) -> Result<BufferOpenResult, RpcError> {
    // Composite pre-step (docs/protocol-composites.md, A): record the jump origin onto this
    // client's nav history — `nav/record` folded in, so result-style opens are one round-trip.
    if let Some(from) = params.record_nav_from {
        let mut s = state.lock().await;
        if let Some(entry) = nav_entry_for(&s, ctx.client_id, from) {
            s.nav_history
                .entry(ctx.client_id)
                .or_default()
                .record(entry);
        }
    }
    let prime = params.prime_search.clone();
    let prime_options = params.prime_search_options;
    let mut result = buffer_open_inner(state, ctx, params).await?;
    // Composite post-step: prime the opened buffer's search, anchored at the just-jumped
    // cursor so the first match at-or-after lands SELECTED (a grep jump selects the match).
    // The result's cursor is patched to the selection so clients adopt it directly.
    if let Some(query) = prime.filter(|q| !q.is_empty()) {
        if let Some((cursor, summary)) =
            prime_search_for(state, ctx, result.buffer_id, &query, prime_options).await
        {
            result.cursor = cursor;
            result.search_summary = Some(summary);
        }
    }
    Ok(result)
}

/// The `prime_search` post-step of [`buffer_open`]: a `search/set` anchored at the
/// post-open cursor (the `jump_to` hit), so the match lands selected — the anchored prime
/// the TUI/web grep flows always used. Errors are dropped (an invalid pattern simply
/// doesn't prime); the summary goes out as a `search/state_changed` push since the prime
/// rides another method's response. Returns the post-prime cursor (the selection) and the search
/// summary, so the caller can also fold the summary into its own response (the push can lose the
/// race against the buffer switch on the client).
async fn prime_search_for(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    buffer_id: BufferId,
    query: &str,
    options: MatchOptions,
) -> Option<(CursorState, SearchSummary)> {
    let anchor = {
        let s = state.lock().await;
        s.cursors
            .get(&(ctx.client_id, buffer_id))
            .copied()
            .unwrap_or_default()
            .position
    };
    let r = search_set(
        state,
        ctx,
        SearchSetParams {
            buffer_id,
            query: query.to_string(),
            anchor: Some(anchor),
            extend: false,
            from_selection: false,
            options,
        },
    )
    .await
    .ok()?;
    let push = {
        let s = state.lock().await;
        s.clients.get(&ctx.client_id).map(|session| {
            (
                session.outbound.clone(),
                Notification {
                    jsonrpc: JsonRpc,
                    method: SearchStateChanged::NAME.into(),
                    params: serde_json::to_value(&r.summary).unwrap_or(serde_json::Value::Null),
                },
            )
        })
    };
    if let Some((sender, notif)) = push {
        let _ = sender.send(notif).await;
    }
    Some((r.cursor, r.summary))
}

/// The scroll position to seed a freshly-opened viewport with. A `jump_to` open (grep
/// navigation, goto-definition, nav history) deliberately moves the cursor elsewhere, so the
/// scroll the client last recorded for this buffer predates the jump and would frame the wrong
/// region — returning `None` lets the client centre on the jumped cursor with a single subscribe.
/// A plain (re)open with no jump restores the saved scroll, so reopening a file lands where you
/// left it.
fn open_scroll(
    s: &ServerState,
    client_id: Option<ClientId>,
    buffer_id: BufferId,
    jump_to: Option<LogicalPosition>,
) -> Option<ScrollPosition> {
    if jump_to.is_some() {
        return None;
    }
    client_id.and_then(|c| s.last_scroll.get(&(c, buffer_id)).copied())
}

async fn buffer_open_inner(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: BufferOpenParams,
) -> Result<BufferOpenResult, RpcError> {
    // `Option` wrapping is vestigial — every connected client has an id now (assigned at WS
    // accept). Kept locally so the surrounding code (which threads cursor/scroll lookups through
    // `Option<ClientId>`) stays unchanged.
    let client_id = Some(ctx.client_id);
    let active_project_name: String = {
        let s = state.lock().await;
        s.active_project_or_err(ctx.client_id)?.name.clone()
    };

    // Attach-by-id: shortcut path used by the buffer picker (which needs to switch to scratch
    // buffers too, where there's no path to feed the path-keyed open flow). Ignores the path
    // fields; errors if the id isn't live.
    if let Some(buffer_id) = params.buffer_id {
        let mut s = state.lock().await;
        let buf = s
            .buffers
            .get(&buffer_id)
            .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
        let language = buf.language.clone();
        let line_count = buf.line_count();
        let byte_count = buf.byte_count();
        let revision = buf.revision;
        let saved_revision = buf.saved_revision();
        let path = buf.canonical_path.as_ref().map(|p| p.display().to_string());
        let scratch_number = buf.scratch_number;
        let clamped_jump = params.jump_to.map(|jt| motion::clamp_position(buf, jt));
        let cursor = resolve_open_cursor(&mut s, client_id, buffer_id, clamped_jump);
        let scroll = open_scroll(&s, client_id, buffer_id, params.jump_to);
        let mut pushes = pin_buffer_if_requested(&mut s, buffer_id, params.transient);
        let result = BufferOpenResult {
            buffer_id,
            language,
            line_count,
            byte_count,
            revision,
            saved_revision,
            path,
            scratch_number,
            cursor,
            scroll,
            lsp_server: buffer_lsp_server_ref(&s, buffer_id),
            transient: s.buffers[&buffer_id].transient,
            search_summary: None, // set by buffer_open's prime post-step, not here
        };
        s.touch_mru(buffer_id);
        pushes.extend(refresh_buffer_pickers(&mut s));
        drop(s);
        for (sender, notif) in pushes {
            let _ = sender.send(notif).await;
        }
        return Ok(result);
    }

    let canonical = match (params.path_index, params.relative_path.as_deref()) {
        (None, None) => {
            let mut s = state.lock().await;
            let id = s.allocate_buffer_id();
            let scratch_number = s.next_scratch_number(&active_project_name);
            let mut buf = Buffer::scratch(id, params.language.clone(), scratch_number);
            buf.transient = params.transient == Some(true);
            let clamped_jump = params.jump_to.map(|jt| motion::clamp_position(&buf, jt));
            let cursor = resolve_open_cursor(&mut s, client_id, id, clamped_jump);
            let scroll = open_scroll(&s, client_id, id, params.jump_to);
            let result = BufferOpenResult {
                buffer_id: id,
                language: buf.language.clone(),
                line_count: buf.line_count(),
                byte_count: buf.byte_count(),
                revision: 0,
                saved_revision: buf.saved_revision(),
                path: None,
                scratch_number: Some(scratch_number),
                cursor,
                scroll,
                lsp_server: None, // scratch buffers are never language-server-backed
                transient: buf.transient,
                search_summary: None, // set by buffer_open's prime post-step, not here
            };
            s.buffers.insert(id, buf);
            s.buffer_projects.insert(id, active_project_name.clone());
            s.touch_mru(id);
            let pushes = refresh_buffer_pickers(&mut s);
            drop(s);
            for (sender, notif) in pushes {
                let _ = sender.send(notif).await;
            }
            return Ok(result);
        }
        (Some(idx), rel) => {
            let s = state.lock().await;
            let base = s
                .active_project_or_err(ctx.client_id)?
                .paths
                .get(idx as usize)
                .ok_or_else(|| RpcError::invalid_path(format!("path_index {idx} out of range")))?
                .clone();
            drop(s);
            let base_is_file = base.is_file();
            let candidate = match rel {
                None | Some("") => base.clone(),
                Some(r) if base_is_file => {
                    return Err(RpcError::invalid_path(format!(
                        "path_index {idx} is a single-file entry; relative_path must be empty (got {r:?})"
                    )));
                }
                Some(r) => base.join(r),
            };
            // Resolve to a canonical-shaped path. When the target file already exists,
            // straight canonicalize. When `create_if_missing` is set and the file (or even
            // some of its parents — multi-segment paths like `foo/bar/baz.rs`) doesn't
            // exist, walk up to the deepest existing ancestor via `canonicalize_partial`
            // and re-attach the not-yet-existing tail. The file (and any missing parents)
            // is written to disk at the first save; the boundary check below runs against
            // the resolved path either way.
            match std::fs::canonicalize(&candidate) {
                Ok(p) => p,
                Err(_) if params.create_if_missing => {
                    canonicalize_partial(&candidate).map_err(|e| {
                        RpcError::invalid_path(format!(
                            "canonicalizing {}: {e}",
                            candidate.display()
                        ))
                    })?
                }
                Err(e) => {
                    return Err(RpcError::invalid_path(format!(
                        "canonicalizing {}: {e}",
                        candidate.display()
                    )));
                }
            }
        }
        (None, Some(_)) => {
            return Err(RpcError::invalid_params(
                "relative_path provided without path_index",
            ));
        }
    };

    {
        let mut s = state.lock().await;
        if !s.active_project_or_err(ctx.client_id)?.contains(&canonical) {
            return Err(RpcError::invalid_path(format!(
                "{} is outside the project's access boundary",
                canonical.display()
            )));
        }
        if let Some(existing) = s.buffer_for_path_in_project(&active_project_name, &canonical) {
            let buf = &s.buffers[&existing];
            let language = buf.language.clone();
            let line_count = buf.line_count();
            let byte_count = buf.byte_count();
            let revision = buf.revision;
            let saved_revision = buf.saved_revision();
            let clamped_jump = params.jump_to.map(|jt| motion::clamp_position(buf, jt));
            let cursor = resolve_open_cursor(&mut s, client_id, existing, clamped_jump);
            let cursor = match client_id {
                Some(c) => wrap_for_response(&s, c, existing, cursor),
                None => cursor,
            };
            let scroll = open_scroll(&s, client_id, existing, params.jump_to);
            let mut pushes = pin_buffer_if_requested(&mut s, existing, params.transient);
            let result = BufferOpenResult {
                buffer_id: existing,
                language,
                line_count,
                byte_count,
                revision,
                saved_revision,
                path: Some(canonical.display().to_string()),
                scratch_number: None,
                cursor,
                scroll,
                lsp_server: buffer_lsp_server_ref(&s, existing),
                transient: s.buffers[&existing].transient,
                search_summary: None, // set by buffer_open's prime post-step, not here
            };
            s.touch_mru(existing);
            pushes.extend(refresh_buffer_pickers(&mut s));
            drop(s);
            for (sender, notif) in pushes {
                let _ = sender.send(notif).await;
            }
            return Ok(result);
        }
    }

    let mut s = state.lock().await;
    let id = s.allocate_buffer_id();
    let mut buf = if params.create_if_missing && !canonical.exists() {
        // New file: empty buffer with the target path attached. Save will write to disk.
        Buffer::new_at_path(id, canonical.clone(), params.language.clone())
    } else {
        Buffer::load_from_file(id, canonical.clone()).map_err(RpcError::file_io)?
    };
    buf.transient = params.transient == Some(true);
    let clamped_jump = params.jump_to.map(|jt| motion::clamp_position(&buf, jt));
    // First-time open of this buffer: no prior cursor or scroll to surface — but the client could
    // already have one if a previous server-side session allocated state. Look it up anyway for
    // consistency with the reopen path.
    let cursor = resolve_open_cursor(&mut s, client_id, id, clamped_jump);
    // Resolve the Git baseline once (repo discovery + reading the committed blob) and diff the
    // buffer against it, so git-aware views have hunks from the first frame and later edits can
    // re-diff cheaply without touching the repo. Best-effort; untracked / no-repo → empty.
    let git_baseline = crate::git::load_baseline(&canonical);
    let git_unstaged = crate::git::diff_hunks(git_baseline.index_blob.as_deref(), &buf.text);
    let git_both = crate::git::compose_both(&git_baseline.staged_hunks, &git_unstaged);
    s.buffers.insert(id, buf);
    s.buffer_projects.insert(id, active_project_name.clone());
    s.git_baseline.insert(id, git_baseline);
    s.git_unstaged_hunks.insert(id, git_unstaged);
    s.git_both_hunks.insert(id, git_both);

    // LSP: ensure a language server for this file's language and open the document against it.
    // `ensure` returns a launch request when it created a fresh (Starting) handle; we spawn the
    // handshake task after releasing the lock. `notify_open` is a no-op until the server is ready —
    // the launch task opens every registered buffer once the handshake lands.
    let mut lsp_launch: Option<(
        crate::lsp::manager::LspServerKey,
        crate::lsp::config::LspServerSpec,
        u64,
    )> = None;
    if let Some(language) = s.buffers[&id].language.clone() {
        if let Some(spec) = crate::lsp::config::server_spec(&language) {
            let roots = s
                .projects
                .get(&active_project_name)
                .map(|p| p.paths.clone())
                .unwrap_or_default();
            let root = crate::lsp::manager::discover_root(
                &canonical,
                spec.root_markers,
                crate::lsp::config::workspace_marker(&language),
                &roots,
            );
            let key = crate::lsp::manager::LspServerKey {
                root,
                language: language.clone(),
            };
            if let Some(generation) = s.lsp.ensure(&key, spec.command) {
                lsp_launch = Some((key.clone(), spec, generation));
            }
            s.lsp.register_doc(id, &key);
            let uri = crate::lsp::uri::path_to_uri(&canonical);
            let text = s.buffers[&id].text.to_string();
            let version = s.buffers[&id].revision as i64;
            s.lsp.notify_open(id, &key, &uri, &language, version, &text);
        }
    }

    let cursor = match client_id {
        Some(c) => wrap_for_response(&s, c, id, cursor),
        None => cursor,
    };
    let buf = &s.buffers[&id];
    let scroll = open_scroll(&s, client_id, id, params.jump_to);
    let result = BufferOpenResult {
        buffer_id: id,
        language: buf.language.clone(),
        line_count: buf.line_count(),
        byte_count: buf.byte_count(),
        revision: buf.revision,
        saved_revision: buf.saved_revision(),
        path: Some(canonical.display().to_string()),
        scratch_number: None,
        cursor,
        scroll,
        lsp_server: buffer_lsp_server_ref(&s, id),
        transient: buf.transient,
        search_summary: None, // set by buffer_open's prime post-step, not here
    };
    s.touch_mru(id);
    let pushes = refresh_buffer_pickers(&mut s);
    drop(s);
    if let Some((key, spec, generation)) = lsp_launch {
        tokio::spawn(crate::lsp::manager::launch(
            state.clone(),
            key,
            spec,
            generation,
        ));
    }
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    tracing::info!(buffer_id = id, path = %canonical.display(), "buffer opened");
    Ok(result)
}

// ---- git/* -------------------------------------------------------------------------------------

/// Blame for a single buffer line, cursor-driven. Whole-file blame is computed once per buffer
/// revision and cached, so repeated calls as the cursor moves within a revision are O(1) lookups.
/// Best-effort: no repo / untracked file / line past EOF all yield `blame: None` rather than an
/// error, so the client can call this freely without special-casing non-git buffers.
pub async fn git_blame_line(
    state: &SharedState,
    _ctx: &mut ConnectionCtx,
    params: GitBlameLineParams,
) -> Result<GitBlameLineResult, RpcError> {
    let mut s = state.lock().await;
    let buf = s
        .buffers
        .get(&params.buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
    if buf.canonical_path.is_none() {
        return Ok(GitBlameLineResult {
            blame: None,
            commit_info: None,
        }); // scratch buffer
    }
    let revision = buf.revision;

    let stale = s
        .git_blame
        .get(&params.buffer_id)
        .is_none_or(|c| c.revision != revision);
    if stale {
        // Blame via the cached repo (no rediscovery). `None` repo (untracked / no repo) → empty.
        // The `buf`/`git_baseline` borrows end at the `compute_blame` call; `lines` is owned, so
        // the `git_blame` mutation below is free of them.
        let lines = match s
            .git_baseline
            .get(&params.buffer_id)
            .and_then(|b| b.repo.as_ref())
        {
            Some(repo) => crate::git::compute_blame(repo, &buf.text).unwrap_or_default(),
            None => Vec::new(),
        };
        s.git_blame
            .insert(params.buffer_id, BlameCache { revision, lines });
    }

    let blame = s
        .git_blame
        .get(&params.buffer_id)
        .and_then(|c| c.lines.get(params.line as usize).cloned().flatten());
    // Composite post-step (docs/protocol-composites.md, G): resolve the commit's details in
    // the same round-trip. Best-effort, like `git/commit_info`.
    let commit_info = match &blame {
        Some(b) if params.include_commit_info && !b.is_uncommitted => s
            .git_baseline
            .get(&params.buffer_id)
            .and_then(|base| base.repo.as_ref())
            .and_then(|repo| crate::git::commit_info(repo, &b.commit)),
        _ => None,
    };
    Ok(GitBlameLineResult { blame, commit_info })
}

/// Full details for a single commit (the blame "commit details" popover). Best-effort: a buffer
/// with no repo, or a revision that doesn't resolve, yields `info: None` rather than an error.
pub async fn git_commit_info(
    state: &SharedState,
    _ctx: &mut ConnectionCtx,
    params: GitCommitInfoParams,
) -> Result<GitCommitInfoResult, RpcError> {
    let s = state.lock().await;
    if !s.buffers.contains_key(&params.buffer_id) {
        return Err(RpcError::buffer_not_found(params.buffer_id));
    }
    let info = s
        .git_baseline
        .get(&params.buffer_id)
        .and_then(|b| b.repo.as_ref())
        .and_then(|repo| crate::git::commit_info(repo, &params.commit));
    Ok(GitCommitInfoResult { info })
}

/// Toggle the inline diff view for a viewport. Turning it on recomputes the buffer's hunks (they
/// may be stale — Phase-1 computed them at open and edits with the view off don't refresh them),
/// then re-renders the whole window: the phantom rows change the visual-row layout and
/// `max_scroll`, so a full resend (like `viewport/set_wrap`) is simpler and correct.
pub async fn git_set_diff_view(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: GitSetDiffViewParams,
) -> Result<ViewportWindowResult, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    let vp = require_viewport_mut(&mut s, params.viewport_id, client_id)?;
    vp.diff_view = params.enabled;
    let (cols, rows, overscan, wrap, marker_width, tab_width, buffer_id, scroll_line) = (
        vp.cols,
        vp.rows,
        vp.overscan_rows,
        vp.wrap,
        vp.continuation_marker_width,
        vp.tab_width,
        vp.buffer_id,
        vp.scroll_logical_line,
    );

    // Refresh hunks so the first diff frame is accurate; clearing the view leaves them as-is
    // (harmless — nothing renders them).
    if params.enabled {
        recompute_diff_hunks_if_viewed(&mut s, buffer_id);
    }

    let buf = s
        .buffers
        .get(&buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let line_count = buf.line_count();
    let (first, last_excl) = pushed_range(scroll_line, rows, overscan, line_count);
    let search = s.searches.get(&(client_id, buffer_id));
    let hunks = buffer_both_hunks(&s, buffer_id);
    let diagnostics = buffer_diagnostics(&s, buffer_id);
    let buf = &s.buffers[&buffer_id];
    let window = render_window(
        buf,
        first,
        last_excl,
        wrap::WrapGeometry {
            wrap,
            cols,
            marker_width,
            tab_width,
        },
        rows,
        WindowDecorations {
            search,
            diff_view: params.enabled,
            hunks,
            diagnostics,
            git_status: buffer_git_status(&s, buffer_id),
        },
    );

    let vp = s
        .viewports
        .get_mut(&params.viewport_id)
        .expect("just checked");
    vp.first_logical_line = first;
    vp.last_logical_line_exclusive = last_excl;
    Ok(ViewportWindowResult { window })
}

/// Jump the cursor to the start of the next/previous changed region (hunk). Works whether or not
/// the diff view is on, so it recomputes the buffer's hunks fresh — the call is user-initiated and
/// infrequent, so a one-off `git diff` is fine, and it keeps navigation correct even when edits
/// happened with the view off (which skips the per-edit recompute). Returns the (possibly
/// unchanged) cursor and whether it moved.
pub async fn git_navigate_hunk(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: GitNavigateHunkParams,
) -> Result<GitNavigateHunkResult, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    if !s.buffers.contains_key(&params.buffer_id) {
        return Err(RpcError::buffer_not_found(params.buffer_id));
    }
    let key = (client_id, params.buffer_id);
    let current = s.cursors.get(&key).copied().unwrap_or_default();

    // Diff against the cached baselines — cheap (no repo I/O) and correct regardless of whether a
    // viewport is currently driving the per-edit recompute. The anchors are the union of the
    // HEAD and index diffs, so navigation reaches every change the combined view can show —
    // including a region reverted back to HEAD's content but staged differently (in the index
    // diff only).
    let anchors = {
        let buf = &s.buffers[&params.buffer_id];
        let baseline = s.git_baseline.get(&params.buffer_id);
        let head = crate::git::diff_hunks(baseline.and_then(|b| b.blob.as_deref()), &buf.text);
        let unstaged =
            crate::git::diff_hunks(baseline.and_then(|b| b.index_blob.as_deref()), &buf.text);
        let mut anchors: Vec<u32> = head
            .iter()
            .chain(unstaged.iter())
            .map(|h| h.anchor_line)
            .collect();
        anchors.sort_unstable();
        anchors.dedup();
        anchors
    };
    let target = match params.direction {
        HunkDirection::Next => anchors.iter().find(|&&a| a > params.from_line).copied(),
        HunkDirection::Prev => anchors
            .iter()
            .rev()
            .find(|&&a| a < params.from_line)
            .copied(),
    };

    let Some(target_line) = target else {
        let response = wrap_for_response(&s, client_id, params.buffer_id, current);
        return Ok(GitNavigateHunkResult {
            cursor: response,
            moved: false,
        });
    };

    let buf = &s.buffers[&params.buffer_id];
    let position = motion::clamp_position(
        buf,
        LogicalPosition {
            line: target_line,
            col: 0,
        },
    );
    let result = CursorState {
        position,
        anchor: position,
        match_bracket: None,
        grep_position: None,
    };
    s.cursors.insert(key, result);
    s.record_motion(key, current, result);
    s.virtual_col.remove(&key);
    s.clear_tree_selection_history(client_id, params.buffer_id);
    let search_update = collect_cursor_search_update(&mut s, client_id, params.buffer_id);
    let response = wrap_for_response(&s, client_id, params.buffer_id, result);
    drop(s);
    if let Some((sender, notif)) = search_update {
        let _ = sender.send(notif).await;
    }
    Ok(GitNavigateHunkResult {
        cursor: response,
        moved: true,
    })
}

/// Toggle the staged state of — or revert — the change under the cursor (bare cursor → the whole
/// hunk it sits on) or the selected lines (any wider selection, snapped to whole lines). The
/// server resolves the client's cursor/selection authoritatively — no positions in the params.
///
/// - **Toggle** flips the region's staged state, unstaged-first: anything unstaged is staged
///   (index ← buffer, resolved against the index→buffer diff); otherwise the region's staged
///   change is pulled back out (index region ← HEAD, resolved against the HEAD→index diff with
///   the cursor/selection carried from buffer to index coordinates across any unstaged edits).
///   Requires a non-dirty buffer: the index must never hold content that exists nowhere on disk.
///   The result status reports which direction it resolved to.
/// - **Revert** peels the top layer of the HEAD→index→buffer stack as an ordinary undoable
///   buffer edit: unstaged changes revert to the index's content; a staged-only region reverts
///   to HEAD's. Works on a dirty buffer.
pub async fn git_apply_hunk(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: GitApplyHunkParams,
) -> Result<GitApplyHunkResult, RpcError> {
    let client_id = ctx.client_id;
    let buffer_id = params.buffer_id;
    let mut s = state.lock().await;

    // Echo the (wrap-adjusted) current cursor with a non-`Applied` status — mirrors `lsp/format`.
    let outcome = |s: &ServerState, status: ApplyHunkStatus| -> GitApplyHunkResult {
        let cursor = s
            .cursors
            .get(&(client_id, buffer_id))
            .copied()
            .unwrap_or_default();
        GitApplyHunkResult {
            cursor: wrap_for_response(s, client_id, buffer_id, cursor),
            status,
        }
    };

    let buf = s
        .buffers
        .get(&buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let (dirty, line_ending) = (buf.dirty, buf.line_ending);
    let buffer_text = buf.text.to_string();

    let cursor = s
        .cursors
        .get(&(client_id, buffer_id))
        .copied()
        .unwrap_or_default();
    // Bare cursor (the editor's resting single-char selection) addresses the whole hunk; anything
    // wider snaps to its line span and stages/reverts at line granularity.
    let sel = if cursor.is_point() {
        crate::git::HunkSelection::WholeHunkAt(cursor.position.line)
    } else {
        crate::git::HunkSelection::Lines {
            lo: cursor.anchor.line.min(cursor.position.line),
            hi: cursor.anchor.line.max(cursor.position.line),
        }
    };

    let Some(baseline) = s.git_baseline.get(&buffer_id) else {
        return Ok(outcome(&s, ApplyHunkStatus::Unavailable));
    };
    let Some(repo) = baseline.repo.clone() else {
        return Ok(outcome(&s, ApplyHunkStatus::Unavailable));
    };
    let head_blob = baseline.blob.clone();
    let index_blob = baseline.index_blob.clone();

    match params.action {
        HunkAction::Toggle => {
            if dirty {
                return Ok(outcome(&s, ApplyHunkStatus::DirtyBuffer));
            }
            let index_bytes = index_blob.as_deref().unwrap_or(b"");
            // Unstaged-first, mirroring revert's layering: stage anything unstaged in the region
            // (index ← buffer; an untracked file's empty index baseline makes this the hunk-wise
            // `git add`). When the region holds nothing unstaged, pull its staged change back out
            // (index region ← HEAD) — the cursor/selection lives in buffer lines, so carry it to
            // index lines across the unstaged (index→buffer) diff first.
            let staged_merge =
                crate::git::merge_selected(index_bytes, buffer_text.as_bytes(), &sel, true);
            let (merged, status) = match staged_merge {
                Some(content) => (content, ApplyHunkStatus::Staged),
                None => {
                    let unstaged =
                        crate::git::diff_hunks(Some(index_bytes), &s.buffers[&buffer_id].text);
                    let sel_index = match sel {
                        crate::git::HunkSelection::WholeHunkAt(l) => {
                            crate::git::HunkSelection::WholeHunkAt(crate::git::map_line_to_old(
                                &unstaged, l, false,
                            ))
                        }
                        crate::git::HunkSelection::Lines { lo, hi } => {
                            crate::git::HunkSelection::Lines {
                                lo: crate::git::map_line_to_old(&unstaged, lo, false),
                                hi: crate::git::map_line_to_old(&unstaged, hi, true),
                            }
                        }
                    };
                    match crate::git::merge_selected(
                        head_blob.as_deref().unwrap_or(b""),
                        index_bytes,
                        &sel_index,
                        false,
                    ) {
                        Some(content) => (content, ApplyHunkStatus::Unstaged),
                        None => return Ok(outcome(&s, ApplyHunkStatus::NoChange)),
                    }
                }
            };
            // The in-memory baselines are LF-normalized; restore the file's real endings so the
            // index blob matches what a save writes (mirrors `Buffer::save_to_disk`).
            let mut content = merged;
            if line_ending == LineEnding::Crlf {
                content = crate::git::denormalize_crlf(&content);
            }
            if crate::git::write_index_blob(&repo, &content).is_none() {
                return Ok(outcome(&s, ApplyHunkStatus::Unavailable));
            }
            // Reload the baseline and re-push every viewport on the buffer — gutter markers,
            // phantom rows, and the staged/unstaged status-bar counts all just changed.
            let pushes = refresh_git_for_buffer(&mut s, buffer_id);
            let result = outcome(&s, status);
            drop(s);
            for (sender, notif) in pushes {
                let _ = sender.send(notif).await;
            }
            Ok(result)
        }
        HunkAction::Revert => {
            // Peel the top layer of the HEAD→index→buffer change stack: an unstaged change
            // reverts to the index's content; if the selection touches nothing unstaged, a
            // staged-only region (buffer == index ≠ HEAD) reverts to HEAD's. Pressing again on a
            // re-modified region therefore peels unstaged first, then staged. A layer with no
            // blob (untracked, staged whole-file delete) is simply skipped.
            let merged = index_blob
                .as_deref()
                .and_then(|index| {
                    crate::git::merge_selected(index, buffer_text.as_bytes(), &sel, false)
                })
                .or_else(|| {
                    head_blob.as_deref().and_then(|head| {
                        crate::git::merge_selected(head, buffer_text.as_bytes(), &sel, false)
                    })
                });
            let Some(content) = merged else {
                return Ok(outcome(&s, ApplyHunkStatus::NoChange));
            };
            let new_text = String::from_utf8_lossy(&content).into_owned();
            if buffer_text == new_text {
                return Ok(outcome(&s, ApplyHunkStatus::NoChange));
            }

            // Apply as one whole-document replacement (a single undo step) and refresh exactly
            // like `lsp/format` does.
            let buf = &s.buffers[&buffer_id];
            let was_dirty = buf.dirty;
            let old_len = buf.text.len_chars();
            let cursors_before: HashMap<ClientId, CursorState> = s
                .cursors
                .iter()
                .filter_map(|((c, b), cs)| (*b == buffer_id).then_some((*c, *cs)))
                .collect();
            let buf_mut = s.buffers.get_mut(&buffer_id).expect("just checked");
            let revision =
                buf_mut.apply_edit(0, old_len, &new_text, EditKindTag::Revert, cursors_before);

            // Clamp every cursor on the buffer into the reverted rope.
            let cursor_ids: Vec<ClientId> = s
                .cursors
                .keys()
                .filter_map(|(c, b)| (*b == buffer_id).then_some(*c))
                .collect();
            for cid in cursor_ids {
                if let Some(cur) = s.cursors.get(&(cid, buffer_id)).copied() {
                    let clamped = clamp_cursor(&s.buffers[&buffer_id], cur);
                    s.cursors.insert((cid, buffer_id), clamped);
                }
            }
            s.clear_motion_history_for_buffer(buffer_id);
            s.clear_tree_selection_history_for_buffer(buffer_id);
            s.clear_virtual_col_for_buffer(buffer_id);

            let mut search_summary_pushes = promote_transient(&mut s, buffer_id);
            search_summary_pushes.extend(refresh_searches_for_buffer(&mut s, buffer_id));
            let new_line_count = s.buffers[&buffer_id].line_count();
            // Also recomputes the cached hunks, so the pushed gutter markers are post-revert.
            refresh_viewport_ranges_for_buffer(&mut s, buffer_id, new_line_count);
            notify_lsp_change(&mut s, buffer_id);

            let buf_ref = &s.buffers[&buffer_id];
            let mut pushes: PendingPushes = Vec::new();
            for vp in s.viewports.values() {
                if vp.buffer_id != buffer_id {
                    continue;
                }
                let Some(sender) = s.clients.get(&vp.client_id).map(|c| c.outbound.clone()) else {
                    continue;
                };
                let search = s.searches.get(&(vp.client_id, buffer_id));
                pushes.push((
                    sender,
                    build_lines_changed_notif(
                        buf_ref,
                        vp,
                        revision,
                        search,
                        buffer_both_hunks(&s, buffer_id),
                        buffer_diagnostics(&s, buffer_id),
                        buffer_git_status(&s, buffer_id),
                    ),
                ));
            }
            let picker_pushes = maybe_refresh_dirty(&mut s, buffer_id, was_dirty);

            let result = outcome(&s, ApplyHunkStatus::Reverted);
            drop(s);
            for (sender, notif) in pushes {
                let _ = sender.send(notif).await;
            }
            for (sender, notif) in search_summary_pushes {
                let _ = sender.send(notif).await;
            }
            for (sender, notif) in picker_pushes {
                let _ = sender.send(notif).await;
            }
            Ok(result)
        }
    }
}

// ---- lsp/* --------------------------------------------------------------------------------------

/// Status of every language server in the client's active project. Drives the LSP status dialog.
pub async fn lsp_server_status(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    _params: LspServerStatusParams,
) -> Result<LspServerStatusResult, RpcError> {
    let s = state.lock().await;
    let roots = s.active_project_or_err(ctx.client_id)?.paths.clone();
    Ok(LspServerStatusResult {
        servers: s.lsp.status_for_roots(&roots),
    })
}

/// Restart the language server(s) for a language in the client's active project.
pub async fn lsp_restart_server(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: LspRestartServerParams,
) -> Result<(), RpcError> {
    let roots = {
        let s = state.lock().await;
        s.active_project_or_err(ctx.client_id)?.paths.clone()
    };
    crate::lsp::manager::restart(state, &params.language, &roots).await;
    Ok(())
}

/// Everything needed to issue a cursor-positioned LSP request: a cloned client for the buffer's
/// (ready) server, the document URI, and the cursor mapped into the server's position encoding.
struct LspCursorRequest {
    client: crate::lsp::client::LspClient,
    uri: String,
    line: u32,
    character: u32,
    encoding: crate::lsp::position::PositionEncoding,
}

/// Resolve [`LspCursorRequest`] for `client_id`'s cursor in `buffer_id`, or `None` if the buffer
/// isn't file-backed or has no ready language server. Runs under the state lock; the caller must
/// drop the lock before awaiting the request (the LSP round-trip must not hold it).
fn lsp_cursor_request(
    s: &ServerState,
    client_id: ClientId,
    buffer_id: BufferId,
) -> Option<LspCursorRequest> {
    let buf = s.buffers.get(&buffer_id)?;
    let path = buf.canonical_path.as_deref()?;
    let key = s.lsp.doc_server.get(&buffer_id)?;
    let handle = s.lsp.servers.get(key)?;
    if !matches!(handle.status, LspStatus::Ready) {
        return None;
    }
    let client = handle.client.clone()?;
    let encoding = handle.position_encoding;
    let pos = s
        .cursors
        .get(&(client_id, buffer_id))
        .copied()
        .unwrap_or_default()
        .position;
    let line_text = line_text_no_newline(buf, pos.line);
    let character = crate::lsp::position::byte_to_lsp(&line_text, pos.col as usize, encoding);
    Some(LspCursorRequest {
        client,
        uri: crate::lsp::uri::path_to_uri(path),
        line: pos.line,
        character,
        encoding,
    })
}

/// The `(language, workspace_root)` of the language server backing `buffer_id`, if one is attached.
/// Read from the LSP doc routing, so it's correct on first open and reopen alike. The client uses
/// it to show this buffer's server health (servers are keyed by `(language, workspace_root)`).
fn buffer_lsp_server_ref(
    s: &ServerState,
    buffer_id: BufferId,
) -> Option<aether_protocol::lsp::LspServerRef> {
    s.lsp
        .doc_server
        .get(&buffer_id)
        .map(|key| aether_protocol::lsp::LspServerRef {
            language: key.language.clone(),
            workspace_root: key.root.display().to_string(),
        })
}

/// A buffer line's text without its trailing newline; empty if `line` is past the end.
fn line_text_no_newline(buf: &Buffer, line: u32) -> String {
    if line as usize >= buf.text.len_lines() {
        return String::new();
    }
    let mut s: String = buf.text.line(line as usize).chunks().collect();
    while s.ends_with('\n') || s.ends_with('\r') {
        s.pop();
    }
    s
}

/// Hover info at the cursor. Returns empty when there's no ready server or the server has nothing.
pub async fn lsp_hover(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: LspBufferParams,
) -> Result<LspHoverResult, RpcError> {
    let req = {
        let s = state.lock().await;
        lsp_cursor_request(&s, ctx.client_id, params.buffer_id)
    };
    let Some(req) = req else {
        return Ok(LspHoverResult {
            contents: None,
            markdown: false,
        });
    };
    let params_json = serde_json::json!({
        "textDocument": { "uri": req.uri },
        "position": { "line": req.line, "character": req.character },
    });
    let parsed = match req.client.request("textDocument/hover", params_json).await {
        Ok(v) => parse_hover_contents(&v),
        Err(e) => {
            tracing::debug!(error = %e, "lsp hover request failed");
            None
        }
    };
    let (contents, markdown) = match parsed {
        Some((s, md)) => (Some(s), md),
        None => (None, false),
    };
    Ok(LspHoverResult { contents, markdown })
}

/// Definition location for the symbol at the cursor. Returns `None` when there's no ready server
/// or the server resolves nothing.
pub async fn lsp_goto_definition(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: LspBufferParams,
) -> Result<LspGotoDefinitionResult, RpcError> {
    let req = {
        let s = state.lock().await;
        lsp_cursor_request(&s, ctx.client_id, params.buffer_id)
    };
    let Some(req) = req else {
        return Ok(LspGotoDefinitionResult { location: None });
    };
    let params_json = serde_json::json!({
        "textDocument": { "uri": req.uri },
        "position": { "line": req.line, "character": req.character },
    });
    let location = match req
        .client
        .request("textDocument/definition", params_json)
        .await
    {
        Ok(v) => parse_definition(&v, req.encoding),
        Err(e) => {
            tracing::debug!(error = %e, "lsp definition request failed");
            None
        }
    };
    Ok(LspGotoDefinitionResult { location })
}

/// Jump the cursor to the next/previous diagnostic in the buffer. The server holds the diagnostics,
/// so it resolves the target and moves the cursor authoritatively (mirrors [`git_navigate_hunk`]).
pub async fn lsp_navigate_diagnostic(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: LspNavigateDiagnosticParams,
) -> Result<LspNavigateDiagnosticResult, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    if !s.buffers.contains_key(&params.buffer_id) {
        return Err(RpcError::buffer_not_found(params.buffer_id));
    }
    let key = (client_id, params.buffer_id);
    let current = s.cursors.get(&key).copied().unwrap_or_default();

    let target = navigate_diagnostic_target(
        buffer_diagnostics(&s, params.buffer_id),
        params.from_line,
        params.direction,
    );
    let Some(target) = target else {
        let response = wrap_for_response(&s, client_id, params.buffer_id, current);
        return Ok(LspNavigateDiagnosticResult {
            cursor: response,
            moved: false,
        });
    };

    let buf = &s.buffers[&params.buffer_id];
    let position = motion::clamp_position(buf, target);
    let result = CursorState {
        position,
        anchor: position,
        match_bracket: None,
        grep_position: None,
    };
    s.cursors.insert(key, result);
    s.record_motion(key, current, result);
    s.virtual_col.remove(&key);
    s.clear_tree_selection_history(client_id, params.buffer_id);
    let search_update = collect_cursor_search_update(&mut s, client_id, params.buffer_id);
    let response = wrap_for_response(&s, client_id, params.buffer_id, result);
    drop(s);
    if let Some((sender, notif)) = search_update {
        let _ = sender.send(notif).await;
    }
    Ok(LspNavigateDiagnosticResult {
        cursor: response,
        moved: true,
    })
}

/// The position of the nearest diagnostic strictly beyond `from_line` in `direction`, or `None` if
/// there's none that way. Diagnostics are compared by their start position (line then byte column),
/// so the jump lands on the diagnostic's column; like hunk navigation it's line-granular — a second
/// diagnostic on the cursor's own line isn't a separate stop.
fn navigate_diagnostic_target(
    diags: &[crate::lsp::diagnostics::BufferDiagnostic],
    from_line: u32,
    direction: DiagnosticDirection,
) -> Option<LogicalPosition> {
    let mut anchors: Vec<LogicalPosition> = diags.iter().map(|d| d.start).collect();
    anchors.sort_by_key(|p| (p.line, p.col));
    anchors.dedup();
    match direction {
        DiagnosticDirection::Next => anchors.iter().find(|p| p.line > from_line).copied(),
        DiagnosticDirection::Prev => anchors.iter().rev().find(|p| p.line < from_line).copied(),
    }
}

/// Everything needed to issue a whole-document formatting request: the ready server's client, the
/// document URI + negotiated encoding, the buffer revision at request time (to detect a concurrent
/// edit), and the LSP formatting options derived from the buffer's indent style.
struct LspFormatReq {
    client: crate::lsp::client::LspClient,
    uri: String,
    encoding: crate::lsp::position::PositionEncoding,
    revision: Revision,
    tab_size: u32,
    insert_spaces: bool,
}

/// Outcome of resolving a format request before the round-trip — lets `lsp_format` report a
/// specific reason rather than a catch-all.
enum FormatResolve {
    Ready(LspFormatReq),
    /// A server for this language exists but isn't `Ready` yet.
    NotReady,
    /// The attached server crashed or was stopped.
    Unavailable,
    /// No formatter: no server attached / not file-backed, or the ready server doesn't advertise
    /// `documentFormattingProvider`.
    Unsupported,
}

fn lsp_format_resolve(s: &ServerState, buffer_id: BufferId) -> FormatResolve {
    let Some(buf) = s.buffers.get(&buffer_id) else {
        return FormatResolve::Unsupported;
    };
    let Some(path) = buf.canonical_path.as_deref() else {
        return FormatResolve::Unsupported;
    };
    let Some(key) = s.lsp.doc_server.get(&buffer_id) else {
        return FormatResolve::Unsupported;
    };
    let Some(handle) = s.lsp.servers.get(key) else {
        return FormatResolve::Unsupported;
    };
    match handle.status {
        LspStatus::Ready => {}
        LspStatus::Starting | LspStatus::Initializing | LspStatus::Restarting => {
            return FormatResolve::NotReady
        }
        LspStatus::Crashed { .. } | LspStatus::Stopped => return FormatResolve::Unavailable,
    }
    if !handle.document_formatting {
        return FormatResolve::Unsupported;
    }
    let Some(client) = handle.client.clone() else {
        return FormatResolve::Unsupported;
    };
    let (tab_size, insert_spaces) = match buf.indent_style {
        crate::indent::IndentStyle::Tab => (4, false),
        crate::indent::IndentStyle::Spaces(n) => (n as u32, true),
    };
    FormatResolve::Ready(LspFormatReq {
        client,
        uri: crate::lsp::uri::path_to_uri(path),
        encoding: handle.position_encoding,
        revision: buf.revision,
        tab_size,
        insert_spaces,
    })
}

/// Format the whole buffer via `textDocument/formatting`. Resolves the server and captures the
/// document version under the lock, drops the lock for the round-trip, then re-locks and applies
/// the returned edits as a single whole-document replacement (one undo step), re-pushing the
/// affected viewports — mirrors the undo/redo whole-rope-swap path.
pub async fn lsp_format(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: LspBufferParams,
) -> Result<LspFormatResult, RpcError> {
    let client_id = ctx.client_id;
    let buffer_id = params.buffer_id;

    // Echo the current (possibly soft-wrap-adjusted) cursor with a non-`Applied` status.
    let outcome = |s: &ServerState, status: FormatStatus| -> LspFormatResult {
        let cursor = s
            .cursors
            .get(&(client_id, buffer_id))
            .copied()
            .unwrap_or_default();
        LspFormatResult {
            cursor: wrap_for_response(s, client_id, buffer_id, cursor),
            status,
        }
    };

    let resolved = {
        let s = state.lock().await;
        lsp_format_resolve(&s, buffer_id)
    };
    let req = match resolved {
        FormatResolve::Ready(req) => req,
        FormatResolve::NotReady => {
            let s = state.lock().await;
            return Ok(outcome(&s, FormatStatus::NotReady));
        }
        FormatResolve::Unavailable => {
            let s = state.lock().await;
            return Ok(outcome(&s, FormatStatus::Unavailable));
        }
        FormatResolve::Unsupported => {
            let s = state.lock().await;
            return Ok(outcome(&s, FormatStatus::Unsupported));
        }
    };

    let params_json = serde_json::json!({
        "textDocument": { "uri": req.uri },
        "options": { "tabSize": req.tab_size, "insertSpaces": req.insert_spaces },
    });
    let edits = match req
        .client
        .request("textDocument/formatting", params_json)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(error = %e, "lsp format request failed");
            let s = state.lock().await;
            return Ok(outcome(&s, FormatStatus::NoChange));
        }
    };

    let mut s = state.lock().await;
    let Some(buf) = s.buffers.get(&buffer_id) else {
        return Err(RpcError::buffer_not_found(buffer_id));
    };
    // Edits were computed against `req.revision`; if the buffer moved under us, they're stale.
    if buf.revision != req.revision {
        return Ok(outcome(&s, FormatStatus::NoChange));
    }
    let Some(new_text) = apply_lsp_text_edits(&buf.text, &edits, req.encoding) else {
        return Ok(outcome(&s, FormatStatus::NoChange));
    };
    if buf.text == new_text.as_str() {
        return Ok(outcome(&s, FormatStatus::NoChange)); // formatter produced identical text
    }

    // Apply as one whole-document replacement (single undo step), then refresh like undo/redo.
    let was_dirty = buf.dirty;
    let old_len = buf.text.len_chars();
    let cursors_before: HashMap<ClientId, CursorState> = s
        .cursors
        .iter()
        .filter_map(|((c, b), cs)| (*b == buffer_id).then_some((*c, *cs)))
        .collect();
    let buf_mut = s.buffers.get_mut(&buffer_id).expect("just checked");
    let revision = buf_mut.apply_edit(0, old_len, &new_text, EditKindTag::Format, cursors_before);

    // Clamp every cursor on the buffer into the reformatted rope.
    let cursor_ids: Vec<ClientId> = s
        .cursors
        .keys()
        .filter_map(|(c, b)| (*b == buffer_id).then_some(*c))
        .collect();
    for cid in cursor_ids {
        if let Some(cur) = s.cursors.get(&(cid, buffer_id)).copied() {
            let clamped = clamp_cursor(&s.buffers[&buffer_id], cur);
            s.cursors.insert((cid, buffer_id), clamped);
        }
    }
    s.clear_motion_history_for_buffer(buffer_id);
    s.clear_tree_selection_history_for_buffer(buffer_id);
    s.clear_virtual_col_for_buffer(buffer_id);

    let mut search_summary_pushes = promote_transient(&mut s, buffer_id);
    search_summary_pushes.extend(refresh_searches_for_buffer(&mut s, buffer_id));
    let new_line_count = s.buffers[&buffer_id].line_count();
    refresh_viewport_ranges_for_buffer(&mut s, buffer_id, new_line_count);
    notify_lsp_change(&mut s, buffer_id);

    let buf_ref = &s.buffers[&buffer_id];
    let mut pushes: PendingPushes = Vec::new();
    for vp in s.viewports.values() {
        if vp.buffer_id != buffer_id {
            continue;
        }
        let Some(sender) = s.clients.get(&vp.client_id).map(|c| c.outbound.clone()) else {
            continue;
        };
        let search = s.searches.get(&(vp.client_id, buffer_id));
        pushes.push((
            sender,
            build_lines_changed_notif(
                buf_ref,
                vp,
                revision,
                search,
                buffer_both_hunks(&s, buffer_id),
                buffer_diagnostics(&s, buffer_id),
                buffer_git_status(&s, buffer_id),
            ),
        ));
    }
    let picker_pushes = maybe_refresh_dirty(&mut s, buffer_id, was_dirty);

    let result_cursor = s
        .cursors
        .get(&(client_id, buffer_id))
        .copied()
        .unwrap_or_default();
    let result_cursor = wrap_for_response(&s, client_id, buffer_id, result_cursor);
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    for (sender, notif) in search_summary_pushes {
        let _ = sender.send(notif).await;
    }
    for (sender, notif) in picker_pushes {
        let _ = sender.send(notif).await;
    }
    Ok(LspFormatResult {
        cursor: result_cursor,
        status: FormatStatus::Applied,
    })
}

/// Apply an LSP `TextEdit[]` to `text`, returning the resulting full document, or `None` when the
/// array is empty/absent or an edit is malformed. Edits are non-overlapping per the spec; applied
/// in descending start order so earlier byte offsets stay valid.
fn apply_lsp_text_edits(
    text: &ropey::Rope,
    edits: &serde_json::Value,
    encoding: crate::lsp::position::PositionEncoding,
) -> Option<String> {
    let arr = edits.as_array()?;
    if arr.is_empty() {
        return None;
    }
    let mut byte_edits: Vec<(usize, usize, &str)> = Vec::with_capacity(arr.len());
    for e in arr {
        let range = e.get("range")?;
        let new_text = e
            .get("newText")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let sb = lsp_pos_to_byte(text, range.get("start")?, encoding)?;
        let eb = lsp_pos_to_byte(text, range.get("end")?, encoding)?;
        if sb > eb {
            return None;
        }
        byte_edits.push((sb, eb, new_text));
    }
    byte_edits.sort_by(|a, b| b.0.cmp(&a.0));
    let mut out: String = text.to_string();
    for (sb, eb, new) in byte_edits {
        if eb > out.len() || !out.is_char_boundary(sb) || !out.is_char_boundary(eb) {
            return None;
        }
        out.replace_range(sb..eb, new);
    }
    Some(out)
}

/// Convert an LSP position (line + `character` in `encoding`) to an absolute byte offset in `text`.
/// A line at/past the buffer end clamps to the byte length.
fn lsp_pos_to_byte(
    text: &ropey::Rope,
    pos: &serde_json::Value,
    encoding: crate::lsp::position::PositionEncoding,
) -> Option<usize> {
    let line = pos.get("line")?.as_u64()? as usize;
    let character = pos.get("character")?.as_u64()? as u32;
    if line >= text.len_lines() {
        return Some(text.len_bytes());
    }
    let mut line_str: String = text.line(line).chunks().collect();
    while line_str.ends_with('\n') || line_str.ends_with('\r') {
        line_str.pop();
    }
    let byte_in_line = crate::lsp::position::lsp_to_byte(&line_str, character, encoding);
    Some(text.line_to_byte(line) + byte_in_line)
}

/// Flatten an LSP hover `contents` (MarkupContent, MarkedString, or an array of them) to a string
/// plus whether it's Markdown — so the client renders Markdown vs. literal plain text rather than
/// assuming Markdown for everything.
fn parse_hover_contents(v: &serde_json::Value) -> Option<(String, bool)> {
    let (s, markdown) = markup_to_string(v.get("contents")?)?;
    let s = s.trim().to_string();
    (!s.is_empty()).then_some((s, markdown))
}

/// Returns `(text, is_markdown)`. Per LSP: a bare string and `MarkedString` are Markdown;
/// `MarkupContent` carries an explicit `kind` (only `"plaintext"` is not Markdown); a
/// `MarkedString { language, value }` is a code block, fenced so it renders as Markdown.
fn markup_to_string(c: &serde_json::Value) -> Option<(String, bool)> {
    match c {
        serde_json::Value::String(s) => Some((s.clone(), true)),
        serde_json::Value::Object(o) => {
            let value = o.get("value")?.as_str()?;
            if let Some(lang) = o.get("language").and_then(|v| v.as_str()) {
                // Legacy MarkedString { language, value }: a code block → fence it as Markdown.
                Some((format!("```{lang}\n{value}\n```"), true))
            } else {
                // MarkupContent { kind, value }: Markdown unless explicitly plaintext.
                let markdown = o.get("kind").and_then(|v| v.as_str()) != Some("plaintext");
                Some((value.to_string(), markdown))
            }
        }
        // MarkedString[] (legacy) — Markdown if any part is; parts joined as paragraphs.
        serde_json::Value::Array(a) => {
            let parts: Vec<(String, bool)> = a.iter().filter_map(markup_to_string).collect();
            if parts.is_empty() {
                return None;
            }
            let markdown = parts.iter().any(|(_, md)| *md);
            let text = parts
                .into_iter()
                .map(|(s, _)| s)
                .collect::<Vec<_>>()
                .join("\n\n");
            Some((text, markdown))
        }
        _ => None,
    }
}

/// Parse an LSP definition response (`Location`, `Location[]`, `LocationLink[]`, or null) into the
/// first target location, converting its position into the buffer's byte columns.
fn parse_definition(
    v: &serde_json::Value,
    encoding: crate::lsp::position::PositionEncoding,
) -> Option<LspLocation> {
    let first = match v {
        serde_json::Value::Array(a) => a.first()?,
        serde_json::Value::Object(_) => v,
        _ => return None,
    };
    parse_location_entry(first, encoding)
}

/// Parse a single LSP `Location` / `LocationLink` object into a location in editor coordinates.
/// Shared by `parse_definition` (first entry only) and `parse_references` (every entry).
fn parse_location_entry(
    entry: &serde_json::Value,
    encoding: crate::lsp::position::PositionEncoding,
) -> Option<LspLocation> {
    let (uri, range) = if let Some(u) = entry.get("uri") {
        (u.as_str()?, entry.get("range")?)
    } else {
        // LocationLink: prefer the precise selection range, fall back to the full target range.
        let u = entry.get("targetUri")?.as_str()?;
        let range = entry
            .get("targetSelectionRange")
            .or_else(|| entry.get("targetRange"))?;
        (u, range)
    };
    let start = range.get("start")?;
    let line = start.get("line")?.as_u64()? as u32;
    let character = start.get("character")?.as_u64()? as u32;
    let path = crate::lsp::uri::uri_to_path(uri)?;
    let col = target_byte_col(&path, line, character, encoding);
    Some(LspLocation {
        path: path.display().to_string(),
        position: LogicalPosition { line, col },
    })
}

/// Parse an LSP `textDocument/references` response (`Location[]`, `LocationLink[]`, or null) into
/// every reference location, converting positions into the buffer's byte columns. Entries that
/// fail to parse are skipped.
fn parse_references(
    v: &serde_json::Value,
    encoding: crate::lsp::position::PositionEncoding,
) -> Vec<LspLocation> {
    match v {
        serde_json::Value::Array(a) => a
            .iter()
            .filter_map(|e| parse_location_entry(e, encoding))
            .collect(),
        _ => Vec::new(),
    }
}

/// Parse an LSP `textDocument/documentSymbol` response into a flat, depth-first list of symbol
/// candidates for `abs_path`. The response is one of two shapes (server capability dependent):
///
/// - `DocumentSymbol[]` — hierarchical: each carries `name`, `kind`, an optional `detail`
///   (signature), a `selectionRange` (the name span) and `children`. We recurse, recording the
///   nesting `depth` and jumping to `selectionRange.start`.
/// - `SymbolInformation[]` — flat: each carries `name`, `kind`, a `location` (range) and an
///   optional `containerName` (used as `detail`). All at depth 0.
///
/// `null` / unexpected shapes yield no symbols; entries missing a name or position are skipped.
/// Server order is preserved (it's the natural reading / nesting order). Positions convert to byte
/// columns against `abs_path` (read from disk for non-UTF-8 servers, like `parse_location_entry`).
fn parse_document_symbols(
    v: &serde_json::Value,
    abs_path: &str,
    encoding: crate::lsp::position::PositionEncoding,
) -> Vec<picker_state::SymbolCandidate> {
    let serde_json::Value::Array(items) = v else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in items {
        push_symbol(item, abs_path, encoding, 0, &mut out);
    }
    // Flat servers (e.g. vscode-html-language-server) return `SymbolInformation[]` with no
    // `children`, so every symbol lands at depth 0 with the parent merely named in `containerName`.
    // Rebuild the tree from `range` containment so the outline indents. Gated on "nothing nested
    // yet": a hierarchical `DocumentSymbol` response already carries real depths, and the LSP spec
    // warns its `range` needn't reflect the AST — so we trust the explicit `children` tree there and
    // never second-guess it from ranges.
    if out.iter().all(|c| c.depth == 0) {
        assign_depth_by_containment(&mut out);
    }
    out
}

/// Reconstruct nesting depth for a flat symbol list from `range` containment: in document order,
/// a symbol whose range is enclosed by an ancestor's is one level deeper. Sorts into document order
/// first (by start, widest-first on ties) so it's robust to a server that returns symbols
/// out of order, then walks a stack of open ancestors keyed by their end position.
fn assign_depth_by_containment(cands: &mut [picker_state::SymbolCandidate]) {
    cands.sort_by(|a, b| {
        let pos = |p: &LogicalPosition| (p.line, p.col);
        pos(&a.range_start)
            .cmp(&pos(&b.range_start))
            .then(pos(&b.range_end).cmp(&pos(&a.range_end)))
    });
    let mut ancestor_ends: Vec<(u32, u32)> = Vec::new();
    for c in cands.iter_mut() {
        let start = (c.range_start.line, c.range_start.col);
        // Pop ancestors that have already closed at or before this symbol starts.
        while ancestor_ends.last().is_some_and(|&end| start >= end) {
            ancestor_ends.pop();
        }
        c.depth = ancestor_ends.len() as u32;
        ancestor_ends.push((c.range_end.line, c.range_end.col));
    }
}

/// Append one parsed symbol (and, for `DocumentSymbol`, its children) to `out`. Handles both the
/// hierarchical and the flat response shapes by probing for the fields each carries.
fn push_symbol(
    entry: &serde_json::Value,
    abs_path: &str,
    encoding: crate::lsp::position::PositionEncoding,
    depth: u32,
    out: &mut Vec<picker_state::SymbolCandidate>,
) {
    let Some(name) = entry.get("name").and_then(|v| v.as_str()) else {
        return;
    };
    let symbol_kind = entry
        .get("kind")
        .and_then(|v| v.as_u64())
        .map(aether_protocol::picker::SymbolKind::from_lsp)
        .unwrap_or_default();
    let path = std::path::Path::new(abs_path);
    let pos_at = |range: &serde_json::Value, edge: &str| -> Option<LogicalPosition> {
        let p = range.get(edge)?;
        let line = p.get("line")?.as_u64()? as u32;
        let character = p.get("character")?.as_u64()? as u32;
        Some(LogicalPosition {
            line,
            col: target_byte_col(path, line, character, encoding),
        })
    };
    // DocumentSymbol: `selectionRange` is the name, `range` the full extent. SymbolInformation:
    // both live under `location.range`.
    let name_range = entry
        .get("selectionRange")
        .or_else(|| entry.get("range"))
        .or_else(|| entry.get("location").and_then(|l| l.get("range")));
    let full_range = entry
        .get("range")
        .or_else(|| entry.get("location").and_then(|l| l.get("range")))
        .or(name_range);
    let Some(name_pos) = name_range.and_then(|r| pos_at(r, "start")) else {
        return;
    };
    // The enclosing extent for cursor-containment; fall back to a zero-width span at the name.
    let range_start = full_range
        .and_then(|r| pos_at(r, "start"))
        .unwrap_or(name_pos);
    let range_end = full_range.and_then(|r| pos_at(r, "end")).unwrap_or(name_pos);
    // Only `DocumentSymbol.detail` (a signature). We deliberately skip `SymbolInformation`'s
    // `containerName` — it names the enclosing scope, which the reconstructed indentation already
    // shows, so surfacing it here would just duplicate the parent next to every flat-server symbol.
    let detail = entry
        .get("detail")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    out.push(picker_state::SymbolCandidate {
        abs_path: abs_path.to_string(),
        line: name_pos.line,
        col: name_pos.col,
        name: name.to_string(),
        symbol_kind,
        detail,
        depth,
        range_start,
        range_end,
    });
    if let Some(children) = entry.get("children").and_then(|c| c.as_array()) {
        for child in children {
            push_symbol(child, abs_path, encoding, depth + 1, out);
        }
    }
}

/// Convert a target position's `character` (in the server's encoding) to a byte column. For UTF-8
/// the character *is* the byte offset; otherwise read the target line from disk to convert
/// (best-effort — falls back to the raw character if the file can't be read).
fn target_byte_col(
    path: &std::path::Path,
    line: u32,
    character: u32,
    encoding: crate::lsp::position::PositionEncoding,
) -> u32 {
    if matches!(encoding, crate::lsp::position::PositionEncoding::Utf8) {
        return character;
    }
    let Ok(content) = std::fs::read_to_string(path) else {
        return character;
    };
    match content.lines().nth(line as usize) {
        Some(line_str) => crate::lsp::position::lsp_to_byte(line_str, character, encoding) as u32,
        None => character,
    }
}

/// Re-resolve a buffer's Git baseline from disk (HEAD changed externally — commit / checkout /
/// stage), recompute its hunks, invalidate cached blame, and build `viewport/lines_changed`
/// pushes for every viewport on the buffer so the gutter / inline diff refresh live. Called by
/// the file watcher when something under the repo's `.git` changes. Returns the pushes to send
/// after the state lock is released. No-op (empty) for a scratch buffer or a missing buffer.
pub(crate) fn refresh_git_for_buffer(s: &mut ServerState, buffer_id: BufferId) -> PendingPushes {
    let Some(buf) = s.buffers.get(&buffer_id) else {
        return Vec::new();
    };
    let Some(path) = buf.canonical_path.clone() else {
        return Vec::new();
    };
    let revision = buf.revision;

    // Re-read the committed baseline (the expensive part), then re-diff the live buffer against both
    // the HEAD and index blobs (the latter also picks up staging done outside the editor).
    let baseline = crate::git::load_baseline(&path);
    let unstaged = crate::git::diff_hunks(baseline.index_blob.as_deref(), &buf.text);
    let both = crate::git::compose_both(&baseline.staged_hunks, &unstaged);
    s.git_baseline.insert(buffer_id, baseline);
    s.git_unstaged_hunks.insert(buffer_id, unstaged);
    s.git_both_hunks.insert(buffer_id, both);
    s.git_blame.remove(&buffer_id); // committed history changed → recompute on next request

    let buf = &s.buffers[&buffer_id];
    let diagnostics = buffer_diagnostics(s, buffer_id);
    let mut pushes = Vec::new();
    for vp in s.viewports.values() {
        if vp.buffer_id != buffer_id {
            continue;
        }
        let Some(sender) = s.clients.get(&vp.client_id).map(|c| c.outbound.clone()) else {
            continue;
        };
        let search = s.searches.get(&(vp.client_id, buffer_id));
        pushes.push((
            sender,
            build_lines_changed_notif(
                buf,
                vp,
                revision,
                search,
                buffer_both_hunks(s, buffer_id),
                diagnostics,
                buffer_git_status(s, buffer_id),
            ),
        ));
    }
    pushes
}

// ---- buffer/search ------------------------------------------------------------------------------

// ---- search/* ----------------------------------------------------------------------------------

pub const SEARCH_MAX_MATCHES: usize = 10_000;

/// Run `query` against the buffer and produce a fresh `SearchEntry`, honouring `options`:
/// `fixed_string` escapes the query to a literal, `whole_word` wraps it in `\b…\b`, and `case`
/// selects smartcase (case-insensitive unless the query has an uppercase letter), forced-sensitive
/// or forced-insensitive. `multi_line: true` throughout. Zero-width matches are skipped so
/// patterns like `^` don't pin the cursor.
pub fn compute_search_entry(
    buf: &Buffer,
    query: &str,
    options: &MatchOptions,
) -> Result<SearchEntry, RpcError> {
    if query.is_empty() {
        return Ok(SearchEntry {
            query: String::new(),
            options: *options,
            matches: Vec::new(),
            truncated: false,
            last_pushed_index: 0,
        });
    }
    let regex = {
        // Literal queries are escaped first; whole-word then fences the (escaped or raw) pattern
        // with word boundaries. Smartcase reads the *original* query's casing, matching grep and
        // the prior buffer-search behavior.
        let body = if options.fixed_string {
            regex::escape(query)
        } else {
            query.to_string()
        };
        let pattern = if options.whole_word {
            format!(r"\b(?:{body})\b")
        } else {
            body
        };
        let case_insensitive = match options.case {
            CaseMode::Smart => !query.chars().any(|c| c.is_uppercase()),
            CaseMode::Sensitive => false,
            CaseMode::Insensitive => true,
        };
        regex::RegexBuilder::new(&pattern)
            .case_insensitive(case_insensitive)
            .multi_line(true)
            .build()
            .map_err(|e| RpcError::new(ErrorCode::INVALID_PARAMS, format!("invalid regex: {e}")))?
    };
    let mut matches: Vec<(LogicalPosition, LogicalPosition)> = Vec::new();
    let mut truncated = false;
    let len_bytes = buf.text.len_bytes();
    if len_bytes == 0 {
        return Ok(SearchEntry {
            query: query.to_string(),
            options: *options,
            matches,
            truncated,
            last_pushed_index: 0,
        });
    }
    let source: String = buf.text.chunks().collect();
    for m in regex.find_iter(&source) {
        if matches.len() >= SEARCH_MAX_MATCHES {
            truncated = true;
            break;
        }
        if m.start() == m.end() {
            continue;
        }
        matches.push((
            byte_to_logical(buf, m.start()),
            byte_to_logical(buf, m.end()),
        ));
    }
    Ok(SearchEntry {
        query: query.to_string(),
        options: *options,
        matches,
        truncated,
        last_pushed_index: 0,
    })
}

pub async fn search_set(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    mut params: SearchSetParams,
) -> Result<SearchSetResult, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    let buf = s
        .buffers
        .get(&params.buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
    let key = (client_id, params.buffer_id);

    let mut cursor = s.cursors.get(&key).copied().unwrap_or_default();
    // Composite pre-step (docs/protocol-composites.md, H): derive the query from the
    // selection — `Alt-/` searches the selected text literally. Empty selection = no-op.
    let mut effective_query = None;
    if params.from_selection {
        let (start, end) = scope_range(buf, &cursor, CopyScope::Selection);
        let text = buf.text.slice(start..end).to_string();
        if text.is_empty() {
            return Ok(SearchSetResult {
                cursor: wrap_for_response(&s, client_id, params.buffer_id, cursor),
                summary: SearchSummary {
                    buffer_id: params.buffer_id,
                    total: 0,
                    truncated: false,
                    current_index: 0,
                },
                query: None,
            });
        }
        params.query = regex::escape(&text);
        // The selection is already regex-escaped to a literal, so a `fixed_string` option would
        // escape it a second time — clear it (case / whole-word still apply).
        params.options.fixed_string = false;
        effective_query = Some(params.query.clone());
    }
    let (summary, pushes) = if params.query.is_empty() {
        s.searches.remove(&key);
        let summary = SearchSummary {
            buffer_id: params.buffer_id,
            total: 0,
            truncated: false,
            current_index: 0,
        };
        let pushes = collect_viewport_refresh(&s, client_id, params.buffer_id);
        (summary, pushes)
    } else {
        let mut entry = compute_search_entry(buf, &params.query, &params.options)?;
        // If the caller passed an anchor, jump the cursor to the first match at-or-after it
        // (wrapping to the first match if none). This is how incremental search keeps the cursor
        // anchored at `/`-press time across keystrokes.
        if let Some(anchor_pos) = params.anchor {
            let (target, wrapped) = first_match_at_or_after_with_wrap(&entry, anchor_pos);
            if let Some((start, end_excl)) = target {
                let start_char = motion::pos_to_char(buf, start);
                let end_char_excl = motion::pos_to_char(buf, end_excl);
                let last_char = end_char_excl.saturating_sub(1).max(start_char);
                let position = motion::char_to_pos(buf, last_char);
                // `?`-search grows the selection from where `?` was pressed (`anchor_pos`) through
                // the match. A wrap is the exception — extending across the buffer boundary would
                // engulf the whole span, so on wrap we reset to selecting just the match, exactly
                // like `search/next`. Plain `/` always selects just the match.
                let anchor_p = if params.extend && !wrapped {
                    anchor_pos
                } else {
                    motion::char_to_pos(buf, start_char)
                };
                let new_cursor = CursorState {
                    position,
                    anchor: anchor_p,
                    match_bracket: None,
                    grep_position: None,
                };
                let prev_cursor = cursor;
                s.cursors.insert(key, new_cursor);
                s.record_motion(key, prev_cursor, new_cursor);
                s.virtual_col.remove(&key);
                s.clear_tree_selection_history(client_id, params.buffer_id);
                cursor = new_cursor;
            }
        }
        let buf_ref = &s.buffers[&params.buffer_id];
        let summary = summary_for(buf_ref, &entry, params.buffer_id, &cursor);
        entry.last_pushed_index = summary.current_index;
        s.searches.insert(key, entry);
        let pushes = collect_viewport_refresh(&s, client_id, params.buffer_id);
        (summary, pushes)
    };
    // Stamp `match_bracket` + `grep_position` before sending — without this, the freshly-jumped
    // cursor would arrive at the client missing the status-bar indicators that derive from it.
    let cursor = wrap_for_response(&s, client_id, params.buffer_id, cursor);
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    Ok(SearchSetResult {
        cursor,
        summary,
        query: effective_query,
    })
}

/// First match at-or-after `pos`, falling back to the first match in the buffer (a wrap). Returns
/// the match plus whether it was reached by wrapping, so `?`-search can reset its selection on a
/// wrap rather than extending across the buffer boundary.
fn first_match_at_or_after_with_wrap(
    entry: &SearchEntry,
    pos: LogicalPosition,
) -> (Option<(LogicalPosition, LogicalPosition)>, bool) {
    let found = entry
        .matches
        .iter()
        .copied()
        .find(|(start, _)| pos_tuple(*start) >= pos_tuple(pos));
    let wrapped = found.is_none();
    let target = found.or_else(|| entry.matches.first().copied());
    (target, wrapped)
}

pub async fn search_clear(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: SearchClearParams,
) -> Result<(), RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    if !s.buffers.contains_key(&params.buffer_id) {
        return Err(RpcError::buffer_not_found(params.buffer_id));
    }
    s.searches.remove(&(client_id, params.buffer_id));
    let pushes = collect_viewport_refresh(&s, client_id, params.buffer_id);
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    Ok(())
}

pub async fn search_next(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: SearchNavParams,
) -> Result<SearchNavResult, RpcError> {
    search_navigate_counted(state, ctx, params, Direction::Forward).await
}

/// Shared `search/next`+`search/prev` wrapper handling the composite params
/// (docs/protocol-composites.md, I): optional query revive first (skipping the step when it
/// has no matches — same early-out the clients used), then `count` steps.
async fn search_navigate_counted(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: SearchNavParams,
    direction: Direction,
) -> Result<SearchNavResult, RpcError> {
    if let Some(query) = params.set_query.clone() {
        let set = search_set(
            state,
            ctx,
            SearchSetParams {
                buffer_id: params.buffer_id,
                query,
                anchor: None,
                extend: false,
                from_selection: false,
                options: params.options,
            },
        )
        .await?;
        if set.summary.total == 0 {
            return Ok(SearchNavResult {
                cursor: set.cursor,
                summary: set.summary,
            });
        }
    }
    let mut last = None;
    for _ in 0..params.count.max(1) {
        last = Some(search_navigate(state, ctx, params.buffer_id, direction, params.extend).await?);
    }
    Ok(last.expect("count.max(1) iterations"))
}

pub async fn search_prev(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: SearchNavParams,
) -> Result<SearchNavResult, RpcError> {
    search_navigate_counted(state, ctx, params, Direction::Backward).await
}

async fn search_navigate(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    buffer_id: BufferId,
    direction: Direction,
    extend: bool,
) -> Result<SearchNavResult, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    let key = (client_id, buffer_id);
    let buf = s
        .buffers
        .get(&buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let Some(entry) = s.searches.get(&key) else {
        // No active search — return a zero-summary with the current cursor untouched.
        let cursor = s.cursors.get(&key).copied().unwrap_or_default();
        return Ok(SearchNavResult {
            cursor,
            summary: SearchSummary {
                buffer_id,
                total: 0,
                truncated: false,
                current_index: 0,
            },
        });
    };
    if entry.matches.is_empty() {
        let cursor = s.cursors.get(&key).copied().unwrap_or_default();
        return Ok(SearchNavResult {
            cursor,
            summary: summary_for(buf, entry, buffer_id, &cursor),
        });
    }

    // Find the next/prev match relative to the selection's *far edge in the travel direction* — the
    // right end going forward, the left end going backward. This is the cursor/head in the normal
    // case (a selection oriented the way you're travelling), so navigation proceeds from where the
    // cursor is. But using the far edge rather than the head directly means a direction reversal off
    // a match (e.g. `n` then `Alt-n`) steps to the adjacent match instead of re-selecting the one
    // you're on, and a plain `n`/`prev` after a multi-match `Shift`-extend steps off the *whole*
    // selection instead of landing back inside it. Extend uses the same reference, so both paths
    // keep making progress for free.
    let current = s.cursors.get(&key).copied().unwrap_or_default();
    let reference = match direction {
        Direction::Forward => selection_end(&current),
        Direction::Backward => selection_start(&current),
    };
    // Find the match strictly past the reference in the travel direction. If there isn't one we
    // wrap to the far end — and remember that we wrapped, so an extend can reset instead of growing
    // across the boundary (see the orientation block below).
    let found = match direction {
        Direction::Forward => entry
            .matches
            .iter()
            .copied()
            .find(|(start, _)| pos_tuple(*start) > pos_tuple(reference)),
        Direction::Backward => entry
            .matches
            .iter()
            .rev()
            .copied()
            .find(|(start, _)| pos_tuple(*start) < pos_tuple(reference)),
    };
    let wrapped = found.is_none();
    let target = found.or_else(|| match direction {
        Direction::Forward => entry.matches.first().copied(),
        Direction::Backward => entry.matches.last().copied(),
    });
    let Some((start, end_excl)) = target else {
        return Ok(SearchNavResult {
            cursor: current,
            summary: summary_for(buf, entry, buffer_id, &current),
        });
    };

    // Resolve the match's char bounds. We compute the inclusive end here (one char before the
    // exclusive end) using char-index arithmetic, mirroring how `Char` motion does it — that way
    // multi-byte matches stay on char boundaries.
    let start_char = motion::pos_to_char(buf, start);
    let end_char_excl = motion::pos_to_char(buf, end_excl);
    let last_char = end_char_excl.saturating_sub(1).max(start_char);
    // Non-extend re-selects just the match, oriented by travel direction: going forward the anchor
    // sits at the start and the head leads on the last char; going backward they swap so the head
    // leads on the start char (cursor before anchor). The orientation comes purely from `direction`,
    // so a wrap doesn't flip it — a forward `n` that wraps end→start stays forward-oriented, a
    // backward `prev` that wraps start→end stays backward-oriented. Either way the leftmost end is
    // still the match start, so the `selection_start` reference above keeps making progress.
    //
    // Extend pins the anchor and lands the head on the match's near edge in the travel direction —
    // the last char going forward (so the selection covers through the match), the first char going
    // back — re-anchoring via `extend_anchor` so reversing direction grows the selection instead of
    // discarding the span already covered on the far side. A wrap is the exception: growing the
    // anchor across the document boundary would engulf the whole span from the wrapped match through
    // the old position, so on wrap we fall through to the non-extend reset and select just the
    // target match, letting the user start a fresh selection from the far end.
    let start_pos = motion::char_to_pos(buf, start_char);
    let last_pos = motion::char_to_pos(buf, last_char);
    let (anchor_pos, position) = if extend && !wrapped {
        let head = match direction {
            Direction::Forward => last_pos,
            Direction::Backward => start_pos,
        };
        (extend_anchor(&current, head), head)
    } else {
        match direction {
            Direction::Forward => (start_pos, last_pos),
            Direction::Backward => (last_pos, start_pos),
        }
    };
    let new_cursor = CursorState {
        position,
        anchor: anchor_pos,
        match_bracket: None,
        grep_position: None,
    };
    let prev_cursor = s.cursors.get(&key).copied().unwrap_or_default();
    s.cursors.insert(key, new_cursor);
    s.record_motion(key, prev_cursor, new_cursor);
    s.virtual_col.remove(&key);
    s.clear_tree_selection_history(client_id, buffer_id);
    let buf_ref = &s.buffers[&buffer_id];
    let summary = {
        let entry_ref = s.searches.get(&key).expect("active search just confirmed");
        summary_for(buf_ref, entry_ref, buffer_id, &new_cursor)
    };
    let entry_mut = s
        .searches
        .get_mut(&key)
        .expect("active search just confirmed");
    entry_mut.last_pushed_index = summary.current_index;
    let new_cursor = wrap_for_response(&s, client_id, buffer_id, new_cursor);
    Ok(SearchNavResult {
        cursor: new_cursor,
        summary,
    })
}

fn selection_start(c: &CursorState) -> LogicalPosition {
    if pos_tuple(c.anchor) < pos_tuple(c.position) {
        c.anchor
    } else {
        c.position
    }
}

fn selection_end(c: &CursorState) -> LogicalPosition {
    if pos_tuple(c.anchor) > pos_tuple(c.position) {
        c.anchor
    } else {
        c.position
    }
}

/// New anchor for an *extending* move whose cursor lands on `head`. Normally the anchor is kept, so
/// the selection is the usual `[anchor, head]`. But when `head` falls on the opposite side of the
/// anchor from the *current* cursor — a direction reversal across the pivot — keeping the anchor
/// would throw away the span the selection already covered on the old side. In that case we
/// re-anchor to the previous cursor position so the move grows the selection rather than collapsing
/// it across the pivot. The decision is based purely on where `head` lands relative to the current
/// selection, so it's independent of which binding (next/prev, etc.) drove the move.
fn extend_anchor(current: &CursorState, head: LogicalPosition) -> LogicalPosition {
    let a = pos_tuple(current.anchor);
    let c = pos_tuple(current.position);
    let h = pos_tuple(head);
    let crosses_pivot = (c < a && h > a) || (c > a && h < a);
    if crosses_pivot {
        current.position
    } else {
        current.anchor
    }
}

fn pos_tuple(p: LogicalPosition) -> (u32, u32) {
    (p.line, p.col)
}

/// Compute the `SearchSummary` for the given entry and cursor.
fn summary_for(
    buf: &Buffer,
    entry: &SearchEntry,
    buffer_id: BufferId,
    cursor: &CursorState,
) -> SearchSummary {
    let current_index = match_index_for_cursor(buf, entry, cursor);
    SearchSummary {
        buffer_id,
        total: entry.matches.len() as u32,
        truncated: entry.truncated,
        current_index,
    }
}

/// 1-based index of the match whose range exactly equals the cursor's current selection
/// (`anchor == m.start` *and* `cursor == last char of m`), or `0` if no match matches.
/// Single-char matches: the cursor's selection collapses to a 1-char point, and we match it
/// against the match's single char. Comparing both endpoints means the counter only shows
/// when the user is genuinely "on" a match — extending or shrinking the selection drops it.
fn match_index_for_cursor(buf: &Buffer, entry: &SearchEntry, cursor: &CursorState) -> u32 {
    // The counter reflects the match the cursor *head* sits on: a match is "current" when the head
    // falls anywhere within it. This keeps the index live across all the ways a head lands on a
    // match — `/` and `?` entry, `n`/`Alt-n` re-selection, and `Shift-n`/`Shift-Alt-n` extension
    // (where the selection spans several matches but the head rests on one). It's orientation-
    // agnostic by construction, since only the head matters, not which end is the anchor.
    let pos_char = motion::pos_to_char(buf, cursor.position);
    entry
        .matches
        .iter()
        .position(|(start, end_excl)| {
            let m_start_char = motion::pos_to_char(buf, *start);
            let m_end_char = motion::pos_to_char(buf, *end_excl);
            let m_last_char = m_end_char.saturating_sub(1);
            pos_char >= m_start_char && pos_char <= m_last_char
        })
        .map(|i| (i as u32).saturating_add(1))
        .unwrap_or(0)
}

/// Build one `viewport/lines_changed` notification per viewport owned by `client_id` that's
/// subscribed to `buffer_id`. Used to refresh highlights when a search is set or cleared.
fn collect_viewport_refresh(
    s: &ServerState,
    client_id: ClientId,
    buffer_id: BufferId,
) -> PendingPushes {
    let mut pushes = Vec::new();
    let buf = match s.buffers.get(&buffer_id) {
        Some(b) => b,
        None => return pushes,
    };
    let revision = buf.revision;
    let search_entry = s.searches.get(&(client_id, buffer_id));
    let diagnostics = buffer_diagnostics(s, buffer_id);
    for vp in s.viewports.values() {
        if vp.client_id != client_id || vp.buffer_id != buffer_id {
            continue;
        }
        let Some(sender) = s.clients.get(&vp.client_id).map(|c| c.outbound.clone()) else {
            continue;
        };
        let line_count = buf.line_count();
        let new_first = vp.first_logical_line.min(line_count);
        let new_last_excl = vp
            .last_logical_line_exclusive
            .min(line_count)
            .max(new_first);
        let window = render_window(
            buf,
            new_first,
            new_last_excl,
            vp.wrap_geometry(),
            vp.rows,
            WindowDecorations {
                search: search_entry,
                diff_view: vp.diff_view,
                hunks: buffer_both_hunks(s, buffer_id),
                diagnostics,
                git_status: buffer_git_status(s, buffer_id),
            },
        );
        let params = ViewportLinesChangedParams {
            viewport_id: vp.id,
            revision,
            range: LogicalLineRange {
                start_logical_line: vp.first_logical_line,
                end_logical_line_exclusive: vp.last_logical_line_exclusive,
            },
            total_visual_rows: window.total_visual_rows,
            first_visual_row: window.first_visual_row,
            max_line_width: window.max_line_width,
            replacement_lines: window.lines,
            line_count,
            max_scroll_logical_line: window.max_scroll_logical_line,
            git_status: window.git_status,
        };
        pushes.push((
            sender,
            Notification {
                jsonrpc: JsonRpc,
                method: ViewportLinesChanged::NAME.into(),
                params: serde_json::to_value(params).unwrap_or(serde_json::Value::Null),
            },
        ));
    }
    pushes
}

/// After a cursor change for `(client_id, buffer_id)`, build a `search/state_changed`
/// notification with the recomputed `current_index` — but only when a search is active *and*
/// the index actually changed since the last push. The cursor counts as "on" a match whenever its
/// head sits within one (see `match_index_for_cursor`), so the counter stays live as the cursor
/// moves on and off matches, including while a `?`/`Shift-n` selection spans several of them.
fn collect_cursor_search_update(
    s: &mut ServerState,
    client_id: ClientId,
    buffer_id: BufferId,
) -> Option<(mpsc::Sender<Notification>, Notification)> {
    let cursor = s
        .cursors
        .get(&(client_id, buffer_id))
        .copied()
        .unwrap_or_default();
    let buf = s.buffers.get(&buffer_id)?;
    let new_idx = {
        let entry = s.searches.get(&(client_id, buffer_id))?;
        match_index_for_cursor(buf, entry, &cursor)
    };
    let entry = s.searches.get_mut(&(client_id, buffer_id))?;
    if new_idx == entry.last_pushed_index {
        return None;
    }
    entry.last_pushed_index = new_idx;
    let summary = SearchSummary {
        buffer_id,
        total: entry.matches.len() as u32,
        truncated: entry.truncated,
        current_index: new_idx,
    };
    let session = s.clients.get(&client_id)?;
    Some((
        session.outbound.clone(),
        Notification {
            jsonrpc: JsonRpc,
            method: SearchStateChanged::NAME.into(),
            params: serde_json::to_value(&summary).unwrap_or(serde_json::Value::Null),
        },
    ))
}

/// Build the `buffer/state` notification pushes for every client that has a viewport on this
/// buffer. Used by save, reload, and the file-watcher — mutations bump the buffer's `revision`
/// (which clients already learn from `viewport/lines_changed`) and the client derives `dirty`
/// as `revision != saved_revision`, so this notification is only needed when `saved_revision`
/// changes or when the external-change flags flip.
pub(crate) fn collect_buffer_state_pushes(s: &ServerState, buffer_id: BufferId) -> PendingPushes {
    let Some(buf) = s.buffers.get(&buffer_id) else {
        return Vec::new();
    };
    let params = BufferStateParams {
        buffer_id,
        saved_revision: buf.saved_revision(),
        saved_at_unix_ms: buf.last_modified_unix_ms,
        externally_modified: buf.externally_modified,
        externally_deleted: buf.externally_deleted,
        transient: buf.transient,
        // Lets a save-as rename follow to every other client viewing this shared buffer.
        path: buf
            .canonical_path
            .as_ref()
            .map(|p| p.display().to_string()),
    };
    let json = serde_json::to_value(params).unwrap_or(serde_json::Value::Null);
    let mut clients: std::collections::HashSet<ClientId> = std::collections::HashSet::new();
    for vp in s.viewports.values() {
        if vp.buffer_id == buffer_id {
            clients.insert(vp.client_id);
        }
    }
    clients
        .into_iter()
        .filter_map(|cid| {
            let session = s.clients.get(&cid)?;
            Some((
                session.outbound.clone(),
                Notification {
                    jsonrpc: JsonRpc,
                    method: BufferState::NAME.into(),
                    params: json.clone(),
                },
            ))
        })
        .collect()
}

/// Promote a transient buffer to permanent. Called from every buffer-mutation handler (the
/// first edit is what makes a previewed buffer worth keeping) and from `buffer/save`. Returns
/// the `buffer/state` pushes telling viewers the flag flipped; empty when the buffer wasn't
/// transient (the common case) or doesn't exist.
fn promote_transient(s: &mut ServerState, buffer_id: BufferId) -> PendingPushes {
    match s.buffers.get_mut(&buffer_id) {
        Some(buf) if buf.transient => {
            buf.transient = false;
            collect_buffer_state_pushes(s, buffer_id)
        }
        _ => Vec::new(),
    }
}

/// Apply a `buffer/open { transient }` intent to an *existing* buffer: `Some(false)` pins
/// (promotes) it; `Some(true)` / `None` leave it alone — an open never demotes a permanent
/// buffer to transient. Returns the promotion's `buffer/state` pushes (usually empty).
fn pin_buffer_if_requested(
    s: &mut ServerState,
    buffer_id: BufferId,
    transient: Option<bool>,
) -> PendingPushes {
    if transient == Some(false) {
        promote_transient(s, buffer_id)
    } else {
        Vec::new()
    }
}

/// Recompute every active search on this buffer after a mutation. Returns the pushes (search
/// summary notifications) to be sent after dropping the lock. The line-level highlight refresh
/// happens via the existing `viewport/lines_changed` flow (since `render_window` reads the
/// freshly-recomputed entries).
fn refresh_searches_for_buffer(s: &mut ServerState, buffer_id: BufferId) -> PendingPushes {
    let mut pushes = Vec::new();
    if !s.buffers.contains_key(&buffer_id) {
        return pushes;
    }
    let keys: Vec<(ClientId, BufferId)> = s
        .searches
        .keys()
        .filter(|(_, b)| *b == buffer_id)
        .copied()
        .collect();
    for key in keys {
        let query = s.searches[&key].query.clone();
        let options = s.searches[&key].options;
        let buf = &s.buffers[&buffer_id];
        let mut entry = match compute_search_entry(buf, &query, &options) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let cursor = s.cursors.get(&key).copied().unwrap_or_default();
        let summary = summary_for(buf, &entry, buffer_id, &cursor);
        entry.last_pushed_index = summary.current_index;
        s.searches.insert(key, entry);
        if let Some(sender) = s.clients.get(&key.0).map(|c| c.outbound.clone()) {
            pushes.push((
                sender,
                Notification {
                    jsonrpc: JsonRpc,
                    method: SearchStateChanged::NAME.into(),
                    params: serde_json::to_value(&summary).unwrap_or(serde_json::Value::Null),
                },
            ));
        }
    }
    pushes
}

/// Convert a buffer-wide byte offset to a `(line, col_bytes)` position.
fn byte_to_logical(buf: &Buffer, byte_idx: usize) -> aether_protocol::LogicalPosition {
    let char_idx = buf.text.byte_to_char(byte_idx);
    let line_idx = buf.text.char_to_line(char_idx);
    let line_start_char = buf.text.line_to_char(line_idx);
    let char_offset = char_idx - line_start_char;
    let line_slice = buf.text.line(line_idx);
    let col_bytes = line_slice.char_to_byte(char_offset);
    aether_protocol::LogicalPosition {
        line: line_idx as u32,
        col: col_bytes as u32,
    }
}

/// The buffer a client should land on after its current one is closed: the top of its active
/// project's MRU, else any remaining buffer in that project, else `None` (caller opens a scratch).
/// Shared by `buffer/close` and the deletion paths so the requesting client and any other clients
/// that were viewing the buffer resolve their next buffer identically.
fn next_buffer_for_client(s: &ServerState, client_id: ClientId) -> Option<BufferId> {
    let project_name = s.active_project(client_id).map(|p| p.name.clone());
    s.active_project(client_id)
        .and_then(|p| p.mru_buffers.front().copied())
        .or_else(|| {
            project_name.as_deref().and_then(|name| {
                s.buffer_projects
                    .iter()
                    .find(|(_, pname)| pname.as_str() == name)
                    .map(|(id, _)| *id)
            })
        })
}

/// `(client, buffer)` pairs for every client *other than* `except` that currently has a viewport
/// on one of `buffer_ids`. Capture this BEFORE tearing the buffers down — teardown drops the very
/// viewports this reads. One entry per affected client (the buffer of theirs that is closing).
fn clients_viewing_buffers(
    s: &ServerState,
    buffer_ids: &[BufferId],
    except: ClientId,
) -> Vec<(ClientId, BufferId)> {
    let targets: std::collections::HashSet<BufferId> = buffer_ids.iter().copied().collect();
    let mut seen: std::collections::HashSet<ClientId> = std::collections::HashSet::new();
    let mut out = Vec::new();
    for vp in s.viewports.values() {
        if vp.client_id != except && targets.contains(&vp.buffer_id) && seen.insert(vp.client_id) {
            out.push((vp.client_id, vp.buffer_id));
        }
    }
    out
}

/// Build the `buffer/closed` pushes for the clients captured by [`clients_viewing_buffers`],
/// telling each which buffer to switch to. Call AFTER teardown so each next-buffer reflects the
/// settled MRU. Clients that have since disconnected are skipped.
fn buffer_closed_pushes(s: &ServerState, affected: &[(ClientId, BufferId)]) -> PendingPushes {
    affected
        .iter()
        .filter_map(|&(client_id, buffer_id)| {
            let session = s.clients.get(&client_id)?;
            let params = BufferClosedParams {
                buffer_id,
                next_buffer_id: next_buffer_for_client(s, client_id),
            };
            Some((
                session.outbound.clone(),
                Notification {
                    jsonrpc: JsonRpc,
                    method: BufferClosed::NAME.into(),
                    params: serde_json::to_value(params).unwrap_or(serde_json::Value::Null),
                },
            ))
        })
        .collect()
}

// ---- buffer/close -------------------------------------------------------------------------------

/// Close a buffer globally. Drops the buffer from the server, plus all viewports subscribed
/// to it across every client, all per-`(client, buffer)` state (cursors, motion history,
/// virtual col, tree-selection history, search, last scroll), and all MRU references.
/// Refreshes any subscribed Buffers picker so clients see the buffer vanish from the list.
///
/// Closes are unconditional from the server's point of view — the client is expected to ask
/// for confirmation if the buffer is dirty.
pub async fn buffer_close(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: BufferCloseParams,
) -> Result<aether_protocol::buffer::BufferCloseResult, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    if !s.buffers.contains_key(&params.buffer_id) {
        return Err(RpcError::buffer_not_found(params.buffer_id));
    }
    // Any *other* client viewing this buffer is about to have it pulled out from under it — capture
    // them before teardown drops their viewports, so we can tell them to switch (see below).
    let affected = clients_viewing_buffers(&s, &[params.buffer_id], client_id);
    // Canonical teardown (drops the buffer + all its per-client slices, sends LSP `didClose`,
    // clears diagnostics, and tears down the language server if this was its last buffer).
    let stopped_server = s.close_buffer(params.buffer_id);
    // Pick the next buffer for the requesting client: top of the active project's MRU after
    // cleanup, or — if that's empty — any remaining buffer in the project. The client uses this
    // to attach without an extra RPC round-trip.
    let next_buffer_id = next_buffer_for_client(&s, client_id);
    let mut pushes = refresh_buffer_pickers(&mut s);
    // Tell the other clients their active buffer vanished (each switches to its own next buffer).
    pushes.extend(buffer_closed_pushes(&s, &affected));
    // If closing this buffer shut its language server down, refresh any open "LSP servers"
    // picker so the now-gone server drops out of the list.
    if stopped_server.is_some() {
        pushes.extend(refresh_lsp_server_pickers(&mut s));
    }
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    tracing::info!(buffer_id = params.buffer_id, "buffer closed");
    // Composite post-step (docs/protocol-composites.md, B): attach the client to its next
    // buffer (or a fresh scratch) in the same round-trip.
    let opened = if params.open_next {
        Some(
            buffer_open(
                state,
                ctx,
                BufferOpenParams {
                    buffer_id: next_buffer_id,
                    ..Default::default()
                },
            )
            .await?,
        )
    } else {
        None
    };
    Ok(aether_protocol::buffer::BufferCloseResult {
        next_buffer_id,
        opened,
    })
}

// ---- nav (jump list) ----------------------------------------------------------------------------

/// Map a buffer's canonical path to a `(path_index, relative_path)` within the client's active
/// project, so a nav entry can reopen the file even after it's been closed. `(None, None)` for a
/// scratch buffer (no path) or a buffer outside the active project's roots.
fn buffer_path_ref(
    s: &ServerState,
    client_id: ClientId,
    buffer_id: BufferId,
) -> (Option<u32>, Option<String>) {
    let Some(canonical) = s
        .buffers
        .get(&buffer_id)
        .and_then(|b| b.canonical_path.clone())
    else {
        return (None, None);
    };
    let Some(project) = s.active_project(client_id) else {
        return (None, None);
    };
    for (i, root) in project.paths.iter().enumerate() {
        if canonical == *root {
            return (Some(i as u32), Some(String::new())); // single-file root, or the root itself
        }
        if let Ok(rel) = canonical.strip_prefix(root) {
            return (Some(i as u32), Some(rel.to_string_lossy().into_owned()));
        }
    }
    (None, None)
}

/// The client's current location as a nav entry: the cursor it holds on `buffer_id` plus a
/// reopenable path ref. The buffer is supplied by the client (not inferred from a viewport, since
/// clients may hold several). `None` if that buffer no longer exists.
fn nav_entry_for(s: &ServerState, client_id: ClientId, buffer_id: BufferId) -> Option<NavEntry> {
    if !s.buffers.contains_key(&buffer_id) {
        return None;
    }
    let cursor = s
        .cursors
        .get(&(client_id, buffer_id))
        .copied()
        .unwrap_or_default();
    let (path_index, relative_path) = buffer_path_ref(s, client_id, buffer_id);
    Some(NavEntry {
        buffer_id,
        path_index,
        relative_path,
        cursor,
    })
}

/// Open `entry`'s buffer (reopening a closed file by path, else attaching by id) and restore its
/// full cursor/selection — clamped to the buffer's current bounds — *without* recording a motion
/// in the per-buffer `z` history. Shared by `nav/back`/`nav/forward` and `nav/goto`.
async fn navigate_to(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    entry: NavEntry,
) -> Result<BufferOpenResult, RpcError> {
    // Prefer reopening by path (survives a close); fall back to the id (the only handle a scratch
    // buffer has). `jump_to` is left unset — we restore the full selection below, not a point.
    let by_path = entry.path_index.is_some() || entry.relative_path.is_some();
    let open_params = BufferOpenParams {
        buffer_id: if by_path { None } else { Some(entry.buffer_id) },
        path_index: entry.path_index,
        relative_path: entry.relative_path.clone(),
        language: None,
        create_if_missing: false,
        jump_to: None,
        // Stepping history through a since-closed file is a revisit, not a keep: reopen it
        // transient so walking the jump list doesn't re-accumulate buffers. No effect when the
        // buffer is still open (an open never demotes).
        transient: Some(true),
        record_nav_from: None,
        prime_search: None,
        prime_search_options: MatchOptions::default(),
    };
    let mut result = buffer_open(state, ctx, open_params).await?;

    let mut s = state.lock().await;
    let restored = match s.buffers.get(&result.buffer_id) {
        Some(buf) => CursorState {
            position: motion::clamp_position(buf, entry.cursor.position),
            anchor: motion::clamp_position(buf, entry.cursor.anchor),
            match_bracket: None,
            grep_position: None,
        },
        None => entry.cursor,
    };
    // Direct insert, *not* via record_motion — a jump-back must not feed `z` (see docs/nav design).
    s.cursors
        .insert((ctx.client_id, result.buffer_id), restored);
    result.cursor = restored;
    Ok(result)
}

/// `nav/record` — snapshot the client's current location onto the back stack. The client only
/// calls this for a navigation that actually moves, so recording is unconditional bar the
/// duplicate-top collapse in [`NavHistory::record`].
pub async fn nav_record(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: NavRecordParams,
) -> Result<NavRecordResult, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    let Some(entry) = nav_entry_for(&s, client_id, params.buffer_id) else {
        return Ok(NavRecordResult { recorded: false });
    };
    let recorded = s.nav_history.entry(client_id).or_default().record(entry);
    Ok(NavRecordResult { recorded })
}

/// Shared back/forward step: pop the chosen stack (skipping unrecoverable entries — a closed
/// scratch), push the current location onto the other stack, and navigate to the popped entry.
async fn nav_step(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    current_buffer: BufferId,
    forward: bool,
) -> Result<NavStepResult, RpcError> {
    let client_id = ctx.client_id;
    let chosen: Option<NavEntry> = {
        let mut s = state.lock().await;
        let current = nav_entry_for(&s, client_id, current_buffer);
        let mut chosen = None;
        loop {
            let popped = s.nav_history.get_mut(&client_id).and_then(|h| {
                if forward {
                    h.forward.pop()
                } else {
                    h.back.pop()
                }
            });
            let Some(entry) = popped else { break };
            // A file entry can always be reopened; a scratch entry only if it's still open.
            let resolvable = entry.path_index.is_some()
                || entry.relative_path.is_some()
                || s.buffers.contains_key(&entry.buffer_id);
            if resolvable {
                chosen = Some(entry);
                break;
            }
        }
        if chosen.is_some() {
            if let Some(cur) = current {
                let hist = s.nav_history.entry(client_id).or_default();
                let other = if forward {
                    &mut hist.back
                } else {
                    &mut hist.forward
                };
                other.push(cur);
                if other.len() > crate::state::NAV_HISTORY_CAP {
                    other.remove(0);
                }
            }
        }
        chosen
    };
    match chosen {
        Some(entry) => Ok(NavStepResult {
            target: Some(navigate_to(state, ctx, entry).await?),
        }),
        None => Ok(NavStepResult { target: None }),
    }
}

pub async fn nav_back(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: NavStepParams,
) -> Result<NavStepResult, RpcError> {
    nav_step(state, ctx, params.buffer_id, false).await
}

pub async fn nav_forward(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: NavStepParams,
) -> Result<NavStepResult, RpcError> {
    nav_step(state, ctx, params.buffer_id, true).await
}

/// `nav/goto` — restore a stored entry without touching the server-side stacks. The web client
/// owns its back/forward stacks (native browser history); this just performs the navigation.
pub async fn nav_goto(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: NavGotoParams,
) -> Result<NavStepResult, RpcError> {
    let entry = NavEntry {
        buffer_id: params.buffer_id.unwrap_or(0),
        path_index: params.path_index,
        relative_path: params.relative_path,
        cursor: params.cursor,
    };
    Ok(NavStepResult {
        target: Some(navigate_to(state, ctx, entry).await?),
    })
}

// ---- buffer/save --------------------------------------------------------------------------------

pub async fn buffer_copy(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: BufferCopyParams,
) -> Result<BufferCopyResult, RpcError> {
    let client_id = ctx.client_id;
    let s = state.lock().await;
    let buf = s
        .buffers
        .get(&params.buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
    let cursor = s
        .cursors
        .get(&(client_id, params.buffer_id))
        .copied()
        .unwrap_or_default();
    let (start, end) = scope_range(buf, &cursor, params.scope);
    let text = buf.text.slice(start..end).to_string();
    Ok(BufferCopyResult { text })
}

pub async fn buffer_cut(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: BufferCopyParams,
) -> Result<BufferCutResult, RpcError> {
    let client_id = ctx.client_id;

    // Extract the text and compute the range while holding the lock; then apply the deletion via
    // `Buffer::apply_edit` (which handles the undo entry and tree update) and broadcast.
    let mut s = state.lock().await;
    let cursor = s
        .cursors
        .get(&(client_id, params.buffer_id))
        .copied()
        .unwrap_or_default();
    let buf_ref = s
        .buffers
        .get(&params.buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
    let (start_char, end_char) = scope_range(buf_ref, &cursor, params.scope);
    let text = buf_ref.text.slice(start_char..end_char).to_string();
    let start_pos = motion::char_to_pos(buf_ref, start_char);
    let end_pos_exclusive = motion::char_to_pos(buf_ref, end_char);
    let old_first_line = start_pos.line;
    let old_last_line_excl = end_pos_exclusive.line.saturating_add(1);

    let cursors_before: HashMap<ClientId, CursorState> = s
        .cursors
        .iter()
        .filter_map(|((c, b), cs)| {
            if *b == params.buffer_id {
                Some((*c, *cs))
            } else {
                None
            }
        })
        .collect();

    let buf_mut = s.buffers.get_mut(&params.buffer_id).expect("just checked");
    let was_dirty = buf_mut.dirty;
    let revision = buf_mut.apply_edit(
        start_char,
        end_char,
        "",
        EditKindTag::Delete,
        cursors_before,
    );
    let new_pos = motion::char_to_pos(buf_mut, start_char);
    let new_cursor = CursorState {
        position: new_pos,
        anchor: new_pos,
        match_bracket: None,
        grep_position: None,
    };
    s.cursors.insert((client_id, params.buffer_id), new_cursor);
    s.clear_motion_history_for_buffer(params.buffer_id);
    s.clear_tree_selection_history_for_buffer(params.buffer_id);
    s.clear_virtual_col_for_buffer(params.buffer_id);

    let mut search_summary_pushes = promote_transient(&mut s, params.buffer_id);
    search_summary_pushes.extend(refresh_searches_for_buffer(&mut s, params.buffer_id));
    let new_line_count = s.buffers[&params.buffer_id].line_count();
    refresh_viewport_ranges_for_buffer(&mut s, params.buffer_id, new_line_count);
    let buf_ref = &s.buffers[&params.buffer_id];

    let mut pushes: PendingPushes = Vec::new();
    for vp in s.viewports.values() {
        if vp.buffer_id != params.buffer_id {
            continue;
        }
        if !vp.diff_view
            && !ranges_overlap(
                vp.first_logical_line,
                vp.last_logical_line_exclusive,
                old_first_line,
                old_last_line_excl,
            )
        {
            continue;
        }
        let Some(sender) = s.clients.get(&vp.client_id).map(|c| c.outbound.clone()) else {
            continue;
        };
        let search = s.searches.get(&(vp.client_id, params.buffer_id));
        pushes.push((
            sender,
            build_lines_changed_notif(
                buf_ref,
                vp,
                revision,
                search,
                buffer_both_hunks(&s, params.buffer_id),
                buffer_diagnostics(&s, params.buffer_id),
                buffer_git_status(&s, params.buffer_id),
            ),
        ));
    }

    let picker_pushes = maybe_refresh_dirty(&mut s, params.buffer_id, was_dirty);

    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    for (sender, notif) in search_summary_pushes {
        let _ = sender.send(notif).await;
    }
    for (sender, notif) in picker_pushes {
        let _ = sender.send(notif).await;
    }

    Ok(BufferCutResult {
        text,
        revision,
        cursor: new_cursor,
    })
}

/// Compute the `[start_char, end_char)` range for a copy/cut scope.
fn scope_range(buf: &Buffer, cursor: &CursorState, scope: CopyScope) -> (usize, usize) {
    match scope {
        CopyScope::Selection => {
            // The selection always covers at least 1 char (point: anchor == position). The
            // inclusive endpoint extension by 1 produces a non-empty char range.
            let (start_pos, end_pos) = motion::ordered(cursor.position, cursor.anchor);
            let start = motion::pos_to_char(buf, start_pos);
            let end = motion::pos_to_char(buf, end_pos);
            (start, (end + 1).min(buf.text.len_chars()))
        }
        CopyScope::Line => {
            let line = cursor.position.line as usize;
            let start = buf.text.line_to_char(line);
            let end = if line + 1 < buf.text.len_lines() {
                buf.text.line_to_char(line + 1)
            } else {
                buf.text.len_chars()
            };
            (start, end)
        }
    }
}

/// Canonicalize a path that may not fully exist on disk: walk up to the deepest existing
/// ancestor, canonicalize that, then re-attach the not-yet-created tail components. Used by
/// `buffer_save`'s save-as path so we can boundary-check a yet-to-be-created subdirectory
/// before actually creating it.
///
/// Symlinks in the existing portion are resolved (standard `canonicalize` behaviour); the tail
/// is appended verbatim. Errors only on I/O other than `NotFound`, or when we walk all the way
/// up to a path with no parent.
fn canonicalize_partial(path: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = path.to_path_buf();
    loop {
        match std::fs::canonicalize(&cursor) {
            Ok(canon) => {
                let mut out = canon;
                // suffix was accumulated tail-first; reverse on attach.
                for component in suffix.iter().rev() {
                    out.push(component);
                }
                return Ok(out);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let Some(name) = cursor.file_name().map(|n| n.to_os_string()) else {
                    return Err(e);
                };
                let Some(parent) = cursor.parent().map(|p| p.to_path_buf()) else {
                    return Err(e);
                };
                suffix.push(name);
                cursor = parent;
            }
            Err(e) => return Err(e),
        }
    }
}

pub async fn buffer_save(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: BufferSaveParams,
) -> Result<BufferSaveResult, RpcError> {
    let _client_id = ctx.client_id;

    // Resolve the target absolute path.
    let target: std::path::PathBuf = match (params.path_index, params.relative_path.as_deref()) {
        (None, None) => {
            let s = state.lock().await;
            let buf = s
                .buffers
                .get(&params.buffer_id)
                .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
            buf.canonical_path
                .clone()
                .ok_or_else(RpcError::buffer_has_no_path)?
        }
        (Some(idx), rel) => {
            let s = state.lock().await;
            let base = s
                .active_project_or_err(ctx.client_id)?
                .paths
                .get(idx as usize)
                .ok_or_else(|| RpcError::invalid_path(format!("path_index {idx} out of range")))?
                .clone();
            drop(s);

            let target = match rel {
                None | Some("") => base,
                Some(r) => base.join(r),
            };

            // The target file may not exist yet (creating). Neither may some of its parent
            // directories — `save-as foo/bar/baz.txt` should `mkdir -p foo/bar` rather than
            // erroring. So: resolve the parent by canonicalizing the deepest *existing*
            // ancestor and re-attaching the not-yet-created tail; boundary-check that
            // resolved path *before* any I/O. The actual mkdir-p happens just before the
            // write below (which also covers the in-place save case where the buffer was
            // bound to a multi-segment path via `buffer/open { create_if_missing }`).
            let parent = target.parent().ok_or_else(|| {
                RpcError::invalid_path(format!("{} has no parent directory", target.display()))
            })?;
            let parent_canonical = canonicalize_partial(parent).map_err(|e| {
                RpcError::invalid_path(format!("canonicalizing {}: {e}", parent.display()))
            })?;
            let file_name = target
                .file_name()
                .ok_or_else(|| RpcError::invalid_path("save target has no file name"))?;
            let resolved = parent_canonical.join(file_name);

            let s = state.lock().await;
            if !s.active_project_or_err(ctx.client_id)?.contains(&resolved) {
                return Err(RpcError::invalid_path(format!(
                    "{} is outside the project's access boundary",
                    resolved.display()
                )));
            }
            drop(s);
            resolved
        }
        (None, Some(_)) => {
            return Err(RpcError::invalid_params(
                "relative_path provided without path_index",
            ));
        }
    };

    // Save-as conflict + would-overwrite checks live in the same critical section as the
    // actual write so the existence check can't race with the save (TOCTOU). In v1 single-
    // client this is theoretical, but folding the locks keeps the invariant tidy.
    //
    // Conflict: target path already canonical-bound to a *different* buffer — refuse rather
    // than silently transferring the path. Skipped when target matches the saving buffer's
    // own current path (the in-place save case).
    //
    // Would-overwrite: the file exists on disk but isn't this buffer's current path, and the
    // caller hasn't confirmed. The client retries with `overwrite: true` after asking.
    //
    // I/O happens under the lock; in v1 that's acceptable (single client). For multi-client
    // we'd clone the rope, drop the lock, write, then re-lock to update state.
    let (saved_at_unix_ms, revision) = {
        let mut s = state.lock().await;
        let active_project_name = s.active_project_or_err(ctx.client_id)?.name.clone();
        if let Some(existing) = s.buffer_for_path_in_project(&active_project_name, &target) {
            if existing != params.buffer_id {
                return Err(RpcError::path_owned_by_buffer(existing));
            }
        }
        if !params.overwrite && target.exists() {
            let own_path = s
                .buffers
                .get(&params.buffer_id)
                .and_then(|b| b.canonical_path.as_ref());
            if own_path.map(|p| p.as_path()) != Some(target.as_path()) {
                return Err(RpcError::would_overwrite(target.display()));
            }
        }
        // External-change check: only applies when saving in-place (target matches the buffer's
        // current path). Save-as to a different path is governed by the WOULD_OVERWRITE check
        // above; the buffer's external-change state for its prior path is no longer relevant.
        if !params.overwrite {
            let buf = s
                .buffers
                .get(&params.buffer_id)
                .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
            let saving_in_place = buf
                .canonical_path
                .as_deref()
                .map(|p| p == target.as_path())
                .unwrap_or(false);
            if saving_in_place {
                if buf.externally_deleted {
                    return Err(RpcError::externally_deleted(params.buffer_id));
                }
                if buf.externally_modified {
                    return Err(RpcError::externally_modified(params.buffer_id));
                }
            }
        }
        // Ensure the target's parent dir exists right before the write. Covers both:
        //   - save-as into a new subdir (`save-as foo/bar/baz.txt` with `foo/bar` missing);
        //   - in-place save of a buffer that was bound to a multi-segment path via
        //     `buffer/open { create_if_missing }` (the parent dirs deferred from open).
        // Idempotent when the parent already exists. Boundary check ran earlier — this
        // never creates dirs outside the project.
        if let Some(parent) = target.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent).map_err(RpcError::file_io)?;
            }
        }
        let buf = s
            .buffers
            .get_mut(&params.buffer_id)
            .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
        let saved_at = buf.save_to_disk(target).map_err(RpcError::file_io)?;
        (saved_at, buf.revision)
    };

    // Broadcast buffer/state to all clients with viewports on this buffer, and re-push any
    // open buffer pickers (the dirty flag just flipped off; the path may have moved on Save-As).
    let (state_pushes, picker_pushes) = {
        let mut s = state.lock().await;
        // Saving is a keep-this-buffer signal: promote a transient buffer to permanent. (The
        // first edit normally got there already; this covers a clean-buffer save-as.) The flag
        // rides the buffer/state push below, so we just flip it.
        if let Some(buf) = s.buffers.get_mut(&params.buffer_id) {
            buf.transient = false;
        }
        let state_pushes = collect_buffer_state_pushes(&s, params.buffer_id);
        let picker_pushes = refresh_buffer_pickers(&mut s);
        (state_pushes, picker_pushes)
    };
    let _ = saved_at_unix_ms; // saved_at is captured inside the helper via Buffer::last_modified.
    for (sender, notif) in state_pushes {
        let _ = sender.send(notif).await;
    }
    for (sender, notif) in picker_pushes {
        let _ = sender.send(notif).await;
    }

    Ok(BufferSaveResult {
        saved_at_unix_ms,
        revision,
    })
}

pub async fn buffer_reload(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: BufferReloadParams,
) -> Result<BufferReloadResult, RpcError> {
    let _client_id = ctx.client_id;
    let mut s = state.lock().await;
    if !params.force {
        let buf = s
            .buffers
            .get(&params.buffer_id)
            .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
        if buf.dirty {
            return Err(RpcError::would_discard_changes(params.buffer_id));
        }
    }
    // A user-initiated reload is a keep-this-buffer signal: promote a transient buffer to
    // permanent, like save. Flipped before the reload so its buffer/state push carries the
    // cleared flag. (The watcher's silent reload calls `reload_buffer_locked` directly and
    // deliberately doesn't promote — an external file change shouldn't pin a preview.)
    let mut promoted = false;
    if let Some(buf) = s.buffers.get_mut(&params.buffer_id) {
        promoted = buf.transient;
        buf.transient = false;
    }
    let (result, mut pushes) = reload_buffer_locked(&mut s, params.buffer_id)?;
    if promoted {
        // Reloading a *clean* transient buffer flips no dirty state, so the reload's own
        // picker refresh doesn't fire — re-push open Buffers pickers for the italics change.
        pushes.extend(refresh_buffer_pickers(&mut s));
    }
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    Ok(result)
}

/// Re-read a buffer from disk inside the lock, returning the RPC result and the pushes the
/// caller should emit after dropping the lock. Shared between the `buffer/reload` handler and
/// the file-watcher's silent-reload path.
pub(crate) fn reload_buffer_locked(
    s: &mut ServerState,
    buffer_id: BufferId,
) -> Result<(BufferReloadResult, PendingPushes), RpcError> {
    let was_dirty = s.buffers.get(&buffer_id).map(|b| b.dirty).unwrap_or(false);

    let saved_at_unix_ms = {
        let buf = s
            .buffers
            .get_mut(&buffer_id)
            .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
        if buf.canonical_path.is_none() {
            return Err(RpcError::buffer_has_no_path());
        }
        buf.reload_from_disk().map_err(RpcError::file_io)?
    };

    // Clamp every cursor on this buffer to the new bounds — rope was swapped wholesale.
    let cursor_ids: Vec<ClientId> = s
        .cursors
        .keys()
        .filter_map(|(c, b)| if *b == buffer_id { Some(*c) } else { None })
        .collect();
    {
        let buf = &s.buffers[&buffer_id];
        let clamped: Vec<(ClientId, CursorState)> = cursor_ids
            .iter()
            .filter_map(|cid| {
                let cursor = s.cursors.get(&(*cid, buffer_id)).copied()?;
                Some((*cid, clamp_cursor(buf, cursor)))
            })
            .collect();
        for (cid, cursor) in clamped {
            s.cursors.insert((cid, buffer_id), cursor);
        }
    }
    s.clear_motion_history_for_buffer(buffer_id);
    s.clear_tree_selection_history_for_buffer(buffer_id);
    s.clear_virtual_col_for_buffer(buffer_id);

    let search_summary_pushes = refresh_searches_for_buffer(s, buffer_id);
    let new_line_count = s.buffers[&buffer_id].line_count();
    refresh_viewport_ranges_for_buffer(s, buffer_id, new_line_count);
    // LSP: reload swapped the rope (manual or watcher-driven) — keep the server's analysis fresh.
    notify_lsp_change(s, buffer_id);

    let revision = s.buffers[&buffer_id].revision;
    let buf_ref = &s.buffers[&buffer_id];
    let mut pushes: PendingPushes = Vec::new();
    for vp in s.viewports.values() {
        if vp.buffer_id != buffer_id {
            continue;
        }
        let Some(sender) = s.clients.get(&vp.client_id).map(|c| c.outbound.clone()) else {
            continue;
        };
        let search = s.searches.get(&(vp.client_id, buffer_id));
        pushes.push((
            sender,
            build_lines_changed_notif(
                buf_ref,
                vp,
                revision,
                search,
                buffer_both_hunks(s, buffer_id),
                buffer_diagnostics(s, buffer_id),
                buffer_git_status(s, buffer_id),
            ),
        ));
    }

    let state_pushes = collect_buffer_state_pushes(s, buffer_id);
    let picker_pushes = maybe_refresh_dirty(s, buffer_id, was_dirty);

    pushes.extend(search_summary_pushes);
    pushes.extend(state_pushes);
    pushes.extend(picker_pushes);

    Ok((
        BufferReloadResult {
            revision,
            saved_at_unix_ms: Some(saved_at_unix_ms),
        },
        pushes,
    ))
}

// ---- viewport handlers -------------------------------------------------------------------------

pub async fn viewport_subscribe(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: ViewportSubscribeParams,
) -> Result<ViewportSubscribeResult, RpcError> {
    let client_id = ctx.client_id;

    let mut s = state.lock().await;
    let buf = s
        .buffers
        .get(&params.buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
    let line_count = buf.line_count();
    let buffer_id = buf.id;

    let (first, last_excl) = pushed_range(
        params.scroll.logical_line,
        params.rows,
        params.overscan_rows,
        line_count,
    );
    let search = s.searches.get(&(client_id, params.buffer_id));
    let hunks = buffer_both_hunks(&s, params.buffer_id);
    let diagnostics = buffer_diagnostics(&s, params.buffer_id);
    let buf = &s.buffers[&params.buffer_id];
    // A freshly subscribed viewport starts with the diff view off, but still carries gutter
    // markers (computed from `hunks` regardless of the toggle).
    let window = render_window(
        buf,
        first,
        last_excl,
        wrap::WrapGeometry {
            wrap: params.wrap,
            cols: params.cols,
            marker_width: params.continuation_marker_width,
            tab_width: params.tab_width,
        },
        params.rows,
        WindowDecorations {
            search,
            diff_view: false,
            hunks,
            diagnostics,
            git_status: buffer_git_status(&s, buffer_id),
        },
    );

    let viewport_id = s.allocate_viewport_id();
    let viewport = Viewport {
        id: viewport_id,
        buffer_id,
        client_id,
        cols: params.cols,
        rows: params.rows,
        overscan_rows: params.overscan_rows,
        scroll_logical_line: params.scroll.logical_line,
        scroll_sub_row: params.scroll.sub_row,
        wrap: params.wrap,
        continuation_marker_width: params.continuation_marker_width,
        tab_width: params.tab_width,
        first_logical_line: first,
        last_logical_line_exclusive: last_excl,
        diff_view: false,
    };
    s.viewports.insert(viewport_id, viewport);
    s.last_scroll.insert((client_id, buffer_id), params.scroll);
    tracing::debug!(%client_id, viewport_id, buffer_id, first, last_excl, "viewport subscribed");

    // One logical viewport per client: a new subscribe supersedes the client's previous
    // viewport(s), which the clients historically never unsubscribed. Dropping the stale
    // entries here keeps "has a viewport" meaning "is showing the buffer" — which is also
    // what lets a transient buffer detect it just went hidden and close itself.
    let left_buffers: Vec<BufferId> = {
        let stale: Vec<aether_protocol::ViewportId> = s
            .viewports
            .iter()
            .filter(|(id, v)| v.client_id == client_id && **id != viewport_id)
            .map(|(id, _)| *id)
            .collect();
        let mut buffers: Vec<BufferId> = Vec::new();
        for id in stale {
            if let Some(v) = s.viewports.remove(&id) {
                if v.buffer_id != buffer_id && !buffers.contains(&v.buffer_id) {
                    buffers.push(v.buffer_id);
                }
            }
        }
        buffers
    };
    let (closed, stopped_servers) = s.close_orphaned_transients(left_buffers);
    let mut pushes = Vec::new();
    if !closed.is_empty() {
        for &id in &closed {
            tracing::info!(buffer_id = id, "transient buffer closed (hidden)");
        }
        pushes.extend(refresh_buffer_pickers(&mut s));
    }
    if !stopped_servers.is_empty() {
        pushes.extend(refresh_lsp_server_pickers(&mut s));
    }

    // Snapshot the buffer-level status the client can't derive from the window: external-change
    // flags, diagnostic counts, and language-server health. These otherwise only reach a client via
    // change-notifications (`buffer/state`, `lsp/diagnostics_changed`, `lsp/status_changed`), so a
    // viewport that subscribes *after* the relevant change already happened would show stale state
    // until the next change. Returning it in the response (vs a follow-up push) keeps it atomic with
    // the window and free of any ordering race against the client's editor switch.
    let buf = &s.buffers[&buffer_id];
    let buffer_status = BufferStatusSnapshot {
        externally_modified: buf.externally_modified,
        externally_deleted: buf.externally_deleted,
        diagnostics: diagnostic_counts(buffer_diagnostics(&s, buffer_id)),
        lsp_status: s.lsp.status_for_buffer(buffer_id),
    };
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }

    Ok(ViewportSubscribeResult {
        viewport_id,
        window,
        buffer_status,
    })
}

pub async fn viewport_resize(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: ViewportResizeParams,
) -> Result<ViewportWindowResult, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    let vp = require_viewport_mut(&mut s, params.viewport_id, client_id)?;
    vp.cols = params.cols;
    vp.rows = params.rows;
    let (cols, rows, overscan, wrap, marker_width, tab_width, buffer_id, scroll_line, diff_view) = (
        vp.cols,
        vp.rows,
        vp.overscan_rows,
        vp.wrap,
        vp.continuation_marker_width,
        vp.tab_width,
        vp.buffer_id,
        vp.scroll_logical_line,
        vp.diff_view,
    );

    let buf = s
        .buffers
        .get(&buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let line_count = buf.line_count();
    let (first, last_excl) = pushed_range(scroll_line, rows, overscan, line_count);
    let search = s.searches.get(&(client_id, buffer_id));
    let hunks = buffer_both_hunks(&s, buffer_id);
    let diagnostics = buffer_diagnostics(&s, buffer_id);
    let buf = &s.buffers[&buffer_id];
    let window = render_window(
        buf,
        first,
        last_excl,
        wrap::WrapGeometry {
            wrap,
            cols,
            marker_width,
            tab_width,
        },
        rows,
        WindowDecorations {
            search,
            diff_view,
            hunks,
            diagnostics,
            git_status: buffer_git_status(&s, buffer_id),
        },
    );

    let vp = s
        .viewports
        .get_mut(&params.viewport_id)
        .expect("just checked");
    vp.first_logical_line = first;
    vp.last_logical_line_exclusive = last_excl;
    Ok(ViewportWindowResult { window })
}

pub async fn viewport_scroll_to_row(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: aether_protocol::viewport::ViewportScrollToRowParams,
) -> Result<ViewportWindowResult, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    let vp = require_viewport_mut(&mut s, params.viewport_id, client_id)?;
    let (cols, rows, overscan, wrap, marker_width, tab_width, buffer_id, diff_view) = (
        vp.cols,
        vp.rows,
        vp.overscan_rows,
        vp.wrap,
        vp.continuation_marker_width,
        vp.tab_width,
        vp.buffer_id,
        vp.diff_view,
    );
    let hunks = buffer_both_hunks(&s, buffer_id);
    let buf = s
        .buffers
        .get(&buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let line_count = buf.line_count();
    let deleted_rows = if diff_view {
        deleted_rows_by_anchor(hunks, line_count)
    } else {
        HashMap::new()
    };
    let top_line = logical_line_at_visual_row(
        buf,
        cols,
        wrap,
        marker_width,
        tab_width,
        &deleted_rows,
        params.top_visual_row,
    );
    let (first, last_excl) = pushed_range(top_line, rows, overscan, line_count);
    let search = s.searches.get(&(client_id, buffer_id));
    let hunks = buffer_both_hunks(&s, buffer_id);
    let diagnostics = buffer_diagnostics(&s, buffer_id);
    let buf = &s.buffers[&buffer_id];
    let window = render_window(
        buf,
        first,
        last_excl,
        wrap::WrapGeometry {
            wrap,
            cols,
            marker_width,
            tab_width,
        },
        rows,
        WindowDecorations {
            search,
            diff_view,
            hunks,
            diagnostics,
            git_status: buffer_git_status(&s, buffer_id),
        },
    );
    let vp = s
        .viewports
        .get_mut(&params.viewport_id)
        .expect("just checked");
    vp.scroll_logical_line = top_line;
    vp.scroll_sub_row = 0.0;
    vp.first_logical_line = first;
    vp.last_logical_line_exclusive = last_excl;
    Ok(ViewportWindowResult { window })
}

pub async fn viewport_set_wrap(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: ViewportSetWrapParams,
) -> Result<ViewportWindowResult, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    let vp = require_viewport_mut(&mut s, params.viewport_id, client_id)?;
    vp.wrap = params.wrap;
    let (cols, rows, overscan, wrap, marker_width, tab_width, buffer_id, scroll_line, diff_view) = (
        vp.cols,
        vp.rows,
        vp.overscan_rows,
        vp.wrap,
        vp.continuation_marker_width,
        vp.tab_width,
        vp.buffer_id,
        vp.scroll_logical_line,
        vp.diff_view,
    );

    let buf = s
        .buffers
        .get(&buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let line_count = buf.line_count();
    let (first, last_excl) = pushed_range(scroll_line, rows, overscan, line_count);
    let search = s.searches.get(&(client_id, buffer_id));
    let hunks = buffer_both_hunks(&s, buffer_id);
    let diagnostics = buffer_diagnostics(&s, buffer_id);
    let buf = &s.buffers[&buffer_id];
    let window = render_window(
        buf,
        first,
        last_excl,
        wrap::WrapGeometry {
            wrap,
            cols,
            marker_width,
            tab_width,
        },
        rows,
        WindowDecorations {
            search,
            diff_view,
            hunks,
            diagnostics,
            git_status: buffer_git_status(&s, buffer_id),
        },
    );

    let vp = s
        .viewports
        .get_mut(&params.viewport_id)
        .expect("just checked");
    vp.first_logical_line = first;
    vp.last_logical_line_exclusive = last_excl;
    Ok(ViewportWindowResult { window })
}

pub async fn viewport_scroll(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: ViewportScrollParams,
) -> Result<ViewportWindowResult, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    let vp = require_viewport_mut(&mut s, params.viewport_id, client_id)?;
    vp.scroll_logical_line = params.scroll.logical_line;
    vp.scroll_sub_row = params.scroll.sub_row;
    let (cols, rows, overscan, wrap, marker_width, tab_width, buffer_id, scroll_line, diff_view) = (
        vp.cols,
        vp.rows,
        vp.overscan_rows,
        vp.wrap,
        vp.continuation_marker_width,
        vp.tab_width,
        vp.buffer_id,
        vp.scroll_logical_line,
        vp.diff_view,
    );

    let buf = s
        .buffers
        .get(&buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let line_count = buf.line_count();
    let (first, last_excl) = pushed_range(scroll_line, rows, overscan, line_count);
    let search = s.searches.get(&(client_id, buffer_id));
    let hunks = buffer_both_hunks(&s, buffer_id);
    let diagnostics = buffer_diagnostics(&s, buffer_id);
    let buf = &s.buffers[&buffer_id];
    let window = render_window(
        buf,
        first,
        last_excl,
        wrap::WrapGeometry {
            wrap,
            cols,
            marker_width,
            tab_width,
        },
        rows,
        WindowDecorations {
            search,
            diff_view,
            hunks,
            diagnostics,
            git_status: buffer_git_status(&s, buffer_id),
        },
    );

    let vp = s
        .viewports
        .get_mut(&params.viewport_id)
        .expect("just checked");
    vp.first_logical_line = first;
    vp.last_logical_line_exclusive = last_excl;
    s.last_scroll.insert((client_id, buffer_id), params.scroll);
    Ok(ViewportWindowResult { window })
}

pub async fn viewport_unsubscribe(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: ViewportUnsubscribeParams,
) -> Result<(), RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    let vp = s.viewports.get(&params.viewport_id).ok_or_else(|| {
        RpcError::new(
            ErrorCode::VIEWPORT_NOT_FOUND,
            format!("unknown viewport_id: {}", params.viewport_id),
        )
    })?;
    if vp.client_id != client_id {
        return Err(RpcError::new(
            ErrorCode::VIEWPORT_NOT_FOUND,
            "viewport is not owned by this client",
        ));
    }
    let buffer_id = vp.buffer_id;
    s.viewports.remove(&params.viewport_id);
    // If that was the last viewport on a transient buffer, the buffer just went hidden — close
    // it (same rule as the viewport supersession in `viewport_subscribe`).
    let (closed, stopped_servers) = s.close_orphaned_transients([buffer_id]);
    let mut pushes = Vec::new();
    if !closed.is_empty() {
        pushes.extend(refresh_buffer_pickers(&mut s));
    }
    if !stopped_servers.is_empty() {
        pushes.extend(refresh_lsp_server_pickers(&mut s));
    }
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    Ok(())
}

// ---- helpers -----------------------------------------------------------------------------------

fn require_viewport_mut(
    state: &mut ServerState,
    viewport_id: aether_protocol::ViewportId,
    client_id: ClientId,
) -> Result<&mut Viewport, RpcError> {
    let vp = state.viewports.get_mut(&viewport_id).ok_or_else(|| {
        RpcError::new(
            ErrorCode::VIEWPORT_NOT_FOUND,
            format!("unknown viewport_id: {viewport_id}"),
        )
    })?;
    if vp.client_id != client_id {
        return Err(RpcError::new(
            ErrorCode::VIEWPORT_NOT_FOUND,
            "viewport is not owned by this client",
        ));
    }
    Ok(vp)
}

/// Compute the logical-line range to push for a viewport. Each logical line wraps to >= 1 visual
/// row, so sending `rows + 2*overscan_rows` logical lines is a safe over-approximation of the
/// visible + overscan area.
fn pushed_range(scroll_line: u32, rows: u32, overscan: u32, line_count: u32) -> (u32, u32) {
    let first = scroll_line.saturating_sub(overscan);
    let last_excl = scroll_line
        .saturating_add(rows)
        .saturating_add(overscan)
        .min(line_count);
    (first, last_excl.max(first))
}

/// Recompute every viewport's pushed range for this buffer from `pushed_range` against the new
/// line count. Call **before** building `viewport/lines_changed` notifications after any
/// mutation that may grow or shrink the buffer — otherwise a growth (e.g. undoing a join)
/// leaves the viewport's range clamped to the smaller post-mutation size and the freshly
/// restored lines never reach the client.
fn refresh_viewport_ranges_for_buffer(s: &mut ServerState, buffer_id: BufferId, line_count: u32) {
    for vp in s.viewports.values_mut() {
        if vp.buffer_id != buffer_id {
            continue;
        }
        let (first, last_excl) = pushed_range(
            vp.scroll_logical_line,
            vp.rows,
            vp.overscan_rows,
            line_count,
        );
        vp.first_logical_line = first;
        vp.last_logical_line_exclusive = last_excl;
    }
    recompute_diff_hunks_if_viewed(s, buffer_id);
}

/// Recompute the buffer's diff hunks after a mutation, when any client is viewing it. The gutter
/// change-bar is always on, so any open viewport needs fresh hunks — not just ones with the
/// inline diff view enabled. Called from the post-mutation refresh so every edit path is covered
/// in one place.
///
/// Cheap: it diffs the **cached** baseline against the buffer — no repository discovery or blob
/// read on the keystroke path (those happen in `load_baseline`, on open and on Git changes). It
/// is still a whole-file in-memory diff per edit; debouncing is the next optimisation if that
/// ever bites on very large files.
fn recompute_diff_hunks_if_viewed(s: &mut ServerState, buffer_id: BufferId) {
    let viewed = s.viewports.values().any(|vp| vp.buffer_id == buffer_id);
    if !viewed {
        return;
    }
    let Some(buf) = s.buffers.get(&buffer_id) else {
        return;
    };
    let Some(baseline) = s.git_baseline.get(&buffer_id) else {
        return;
    };
    // Re-diff against the cached index blob — a per-edit in-memory diff with no repo I/O. The
    // combined view recomposes from the cached staged hunks (they only change on git events, not
    // buffer edits) + the fresh unstaged.
    let unstaged = crate::git::diff_hunks(baseline.index_blob.as_deref(), &buf.text);
    let both = crate::git::compose_both(&baseline.staged_hunks, &unstaged);
    s.git_unstaged_hunks.insert(buffer_id, unstaged);
    s.git_both_hunks.insert(buffer_id, both);
}

/// The phantom "deleted" rows each anchor line shows above it, derived from the buffer's diff
/// hunks. Only hunks with removed text (Modified / Deleted) contribute; pure additions have none.
/// A deletion past the last line is clamped onto the final line index, so a newline-terminated
/// file shows it above its trailing empty line. Each row carries its hunk's `DiffStage`; where a
/// staged and an unstaged layer would stack at one anchor (a region modified, staged, then
/// modified again), only the unstaged rows are kept — what's shown deleted is exactly what a
/// revert would restore, and HEAD's text resurfaces once the top layer is staged or reverted.
fn deleted_rows_by_anchor(
    hunks: &[crate::git::DiffHunk],
    line_count: u32,
) -> HashMap<u32, Vec<VirtualRow>> {
    let mut map: HashMap<u32, Vec<VirtualRow>> = HashMap::new();
    let last_line = line_count.saturating_sub(1);
    for h in hunks {
        if h.deleted.is_empty() {
            continue;
        }
        let anchor = h.anchor_line.min(last_line);
        let rows = map.entry(anchor).or_default();
        rows.extend(h.deleted.iter().map(|text| VirtualRow {
            text: text.clone(),
            kind: VirtualRowKind::Deleted,
            stage: h.stage,
        }));
    }
    for rows in map.values_mut() {
        if rows.iter().any(|r| r.stage == DiffStage::Unstaged) {
            rows.retain(|r| r.stage == DiffStage::Unstaged);
        }
    }
    map
}

/// The Git change marker (and its stage) for each affected buffer line, for the gutter
/// change-bar. Added/modified hunks mark their new-side lines `Added`/`Modified`; a pure deletion
/// marks the single line it sits above as `Deleted` (clamped onto the last line for an
/// end-of-buffer deletion), without overriding an Added/Modified marker that's already there.
/// Where a staged and an unstaged hunk cover the same line (modified, staged, modified again),
/// the unstaged layer wins both kind and stage — the line reads as plain unstaged, since that's
/// the content on screen and the layer `git/apply_hunk` acts on.
fn diff_markers_by_line(
    hunks: &[crate::git::DiffHunk],
    line_count: u32,
) -> HashMap<u32, (DiffMarker, DiffStage)> {
    use crate::git::ChangeKind;
    let last_line = line_count.saturating_sub(1);
    let mut map: HashMap<u32, (DiffMarker, DiffStage)> = HashMap::new();
    let put = |map: &mut HashMap<u32, (DiffMarker, DiffStage)>,
               line: u32,
               marker: DiffMarker,
               stage: DiffStage| {
        map.entry(line)
            .and_modify(|(k, s)| {
                // A staged hunk never overrides an unstaged marker (the top layer wins)...
                if *s == DiffStage::Unstaged && stage == DiffStage::Staged {
                    return;
                }
                // ...while Added/Modified outrank a Deleted-above flag (which never downgrades
                // them), and an unstaged write takes the stage with it.
                if marker != DiffMarker::Deleted {
                    *k = marker;
                }
                *s = stage;
            })
            .or_insert((marker, stage));
    };
    for h in hunks {
        match h.kind {
            ChangeKind::Added | ChangeKind::Modified => {
                let marker = if matches!(h.kind, ChangeKind::Added) {
                    DiffMarker::Added
                } else {
                    DiffMarker::Modified
                };
                for line in h.anchor_line..h.anchor_line.saturating_add(h.new_lines) {
                    put(&mut map, line, marker, h.stage);
                }
            }
            ChangeKind::Deleted => {
                put(
                    &mut map,
                    h.anchor_line.min(last_line),
                    DiffMarker::Deleted,
                    h.stage,
                );
            }
        }
    }
    map
}

/// The buffer-wide change summary for the status bar: line counts by change class. `added` /
/// `modified` count the new-side lines of Added / Modified hunks (matching the gutter bars);
/// `deleted` counts the lines a pure deletion removed. A Modified hunk's replaced old-side lines
/// are represented by its `modified` count, not counted again as deletions.
fn git_change_counts(hunks: &[crate::git::DiffHunk]) -> GitChangeCounts {
    use crate::git::ChangeKind;
    let mut c = GitChangeCounts::default();
    for h in hunks {
        match h.kind {
            ChangeKind::Added => c.added += h.new_lines,
            ChangeKind::Modified => c.modified += h.new_lines,
            ChangeKind::Deleted => c.deleted += h.deleted.len() as u32,
        }
    }
    c
}

/// The buffer's *unstaged* diff hunks (vs the index), or an empty slice when none are cached.
fn buffer_unstaged_hunks(s: &ServerState, buffer_id: BufferId) -> &[crate::git::DiffHunk] {
    s.git_unstaged_hunks
        .get(&buffer_id)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// The combined staged+unstaged hunks that drive the gutter / inline diff (each tagged with its
/// `DiffStage`), or an empty slice when none are cached (no repo / untracked / clean).
fn buffer_both_hunks(s: &ServerState, buffer_id: BufferId) -> &[crate::git::DiffHunk] {
    s.git_both_hunks
        .get(&buffer_id)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// Buffer-level Git status for the status bar: branch + staged (HEAD→index) and unstaged
/// (index→buffer) change counts. `Some` for any file inside a repo; `None` otherwise. Staged counts
/// come from the baseline's cached HEAD→index diff; unstaged from the per-edit index→buffer diff.
fn buffer_git_status(s: &ServerState, buffer_id: BufferId) -> Option<GitBufferStatus> {
    let baseline = s.git_baseline.get(&buffer_id)?;
    baseline.repo.as_ref()?; // only file-backed buffers inside a repo carry status
    Some(GitBufferStatus {
        branch: baseline.branch.clone(),
        staged: git_change_counts(&baseline.staged_hunks),
        unstaged: git_change_counts(buffer_unstaged_hunks(s, buffer_id)),
    })
}

/// The buffer's language-server diagnostics, or an empty slice when none are known.
fn buffer_diagnostics(
    s: &ServerState,
    buffer_id: BufferId,
) -> &[crate::lsp::diagnostics::BufferDiagnostic] {
    s.diagnostics
        .get(&buffer_id)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// Notify the language server of a buffer's new full text (LSP `didChange`). Must be called by
/// *every* path that changes buffer text — edits, undo/redo, reload — or the server's analysis
/// (and its diagnostics) goes stale. A no-op unless the buffer is file-backed and open against a
/// ready server; `notify` is a channel send, so it's fire-and-forget under the lock.
fn notify_lsp_change(s: &mut ServerState, buffer_id: BufferId) {
    let Some(buf) = s.buffers.get(&buffer_id) else {
        return;
    };
    let Some(uri) = buf
        .canonical_path
        .as_deref()
        .map(crate::lsp::uri::path_to_uri)
    else {
        return;
    };
    let revision = buf.revision as i64;
    let text = buf.text.to_string();
    s.lsp.notify_change(buffer_id, &uri, revision, &text);
}

/// Build the diagnostics-picker candidates for `buffer_id`: one per diagnostic, sorted top-to-bottom
/// by position, carrying the buffer's path for the `FileAt` jump. Empty if the buffer is gone or has
/// no path.
fn build_diagnostic_candidates(
    s: &ServerState,
    buffer_id: BufferId,
) -> Vec<picker_state::DiagnosticCandidate> {
    let Some(abs_path) = s
        .buffers
        .get(&buffer_id)
        .and_then(|b| b.canonical_path.as_deref())
        .map(|p| p.display().to_string())
    else {
        return Vec::new();
    };
    let mut out: Vec<picker_state::DiagnosticCandidate> = buffer_diagnostics(s, buffer_id)
        .iter()
        .map(|d| picker_state::DiagnosticCandidate {
            line: d.start.line,
            col: d.start.col,
            end_line: d.end.line,
            end_col: d.end.col,
            severity: d.severity,
            message: d.message.clone(),
            abs_path: abs_path.clone(),
        })
        .collect();
    out.sort_by_key(|c| (c.line, c.col));
    out
}

/// Build the references-picker candidates: ask the language server for every reference to the
/// symbol at the cursor (`textDocument/references`, including the declaration), then attach a
/// line-text preview and a display label to each. Returns empty when there's no ready server, the
/// server resolves nothing, or the request fails. Async (off the lock): an LSP round-trip plus
/// reading each referenced file's line from disk.
async fn build_reference_candidates(
    state: &SharedState,
    client_id: ClientId,
    buffer_id: BufferId,
) -> Vec<picker_state::ReferenceCandidate> {
    // Resolve the LSP request and the project roots under the lock, then release it for the I/O.
    let (req, roots) = {
        let s = state.lock().await;
        let roots = s
            .active_project(client_id)
            .map(|p| p.paths.clone())
            .unwrap_or_default();
        (lsp_cursor_request(&s, client_id, buffer_id), roots)
    };
    let Some(req) = req else {
        return Vec::new();
    };
    let params_json = serde_json::json!({
        "textDocument": { "uri": req.uri },
        "position": { "line": req.line, "character": req.character },
        "context": { "includeDeclaration": true },
    });
    let locations = match req
        .client
        .request("textDocument/references", params_json)
        .await
    {
        Ok(v) => parse_references(&v, req.encoding),
        Err(e) => {
            tracing::debug!(error = %e, "lsp references request failed");
            return Vec::new();
        }
    };

    // Cache each referenced file's lines so a file with many references is read only once. `None`
    // marks a file we couldn't read — its previews fall back to empty.
    let mut file_lines: HashMap<String, Option<Vec<String>>> = HashMap::new();
    let mut out: Vec<picker_state::ReferenceCandidate> = locations
        .into_iter()
        // Project-only: a reference that doesn't live under any project root (a dependency, the
        // stdlib, generated code outside the tree) is dropped — `project_relative_parts` is the
        // gate, and its relative path becomes the display label.
        .filter_map(|loc| {
            let (_, display_path) = crate::workspace_index::project_relative_parts(
                std::path::Path::new(&loc.path),
                &roots,
            )?;
            let lines = file_lines.entry(loc.path.clone()).or_insert_with(|| {
                std::fs::read_to_string(&loc.path)
                    .ok()
                    .map(|c| c.lines().map(str::to_string).collect())
            });
            let preview = lines
                .as_ref()
                .and_then(|ls| ls.get(loc.position.line as usize))
                .map(|l| l.trim_end().to_string())
                .unwrap_or_default();
            Some(picker_state::ReferenceCandidate {
                abs_path: loc.path,
                display_path,
                line: loc.position.line,
                col: loc.position.col,
                preview,
            })
        })
        .collect();
    // Stable, file-grouped order: by display path, then position. Dedup identical locations (some
    // servers return the declaration twice, or overlapping ranges collapse to the same start).
    out.sort_by(|a, b| {
        a.display_path
            .cmp(&b.display_path)
            .then_with(|| (a.line, a.col).cmp(&(b.line, b.col)))
    });
    out.dedup_by(|a, b| a.abs_path == b.abs_path && a.line == b.line && a.col == b.col);
    out
}

/// Build the document-symbols-picker candidates: ask the language server for the picked buffer's
/// symbols (`textDocument/documentSymbol`), flattening any hierarchy into a depth-tagged list.
/// Returns the candidates plus the cursor's buffer position (for the initial cursor-enclosing
/// highlight) — both empty/None when there's no ready server, the buffer isn't file-backed, or the
/// request fails. Async (off the lock): one LSP round-trip.
async fn build_symbol_candidates(
    state: &SharedState,
    client_id: ClientId,
    buffer_id: BufferId,
) -> (Vec<picker_state::SymbolCandidate>, Option<LogicalPosition>) {
    let (req, abs_path, cursor) = {
        let s = state.lock().await;
        let abs_path = s
            .buffers
            .get(&buffer_id)
            .and_then(|b| b.canonical_path.clone());
        // The cursor's byte position (for centering on the enclosing symbol), in buffer coords —
        // not the LSP-encoded one in `req`.
        let cursor = s
            .cursors
            .get(&(client_id, buffer_id))
            .map(|c| c.position);
        (lsp_cursor_request(&s, client_id, buffer_id), abs_path, cursor)
    };
    let (Some(req), Some(abs_path)) = (req, abs_path) else {
        return (Vec::new(), None);
    };
    // documentSymbol is whole-document: only the URI is needed (no cursor position).
    let params_json = serde_json::json!({
        "textDocument": { "uri": req.uri },
    });
    let candidates = match req
        .client
        .request("textDocument/documentSymbol", params_json)
        .await
    {
        Ok(v) => parse_document_symbols(&v, &abs_path.display().to_string(), req.encoding),
        Err(e) => {
            tracing::debug!(error = %e, "lsp documentSymbol request failed");
            Vec::new()
        }
    };
    (candidates, cursor)
}

/// Monotonic token minted per async-resolve picker open (References, DocumentSymbols), stored on
/// the picker as `pending_async_load`. Lets a spawned resolve detect that its picker was
/// reset/reopened (a newer epoch) and drop its now-stale result instead of clobbering the current
/// load. Shared across the kinds — they're keyed separately, so the counter just needs uniqueness.
static ASYNC_LOAD_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_async_load_epoch() -> u64 {
    ASYNC_LOAD_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Apply a freshly-resolved async candidate set to its picker, unless a newer open superseded it
/// (the `pending_async_load` epoch moved past `epoch`) or the picker was hidden/closed. Reranks
/// against whatever query the user typed while the resolve was in flight, clamps the window, clears
/// the loading flag, and pushes the updated window. Shared by the References and DocumentSymbols
/// background resolves — both open empty + `ticking` and are filled by this.
async fn apply_async_candidates(
    state: &SharedState,
    client_id: ClientId,
    kind: PickerKind,
    epoch: u64,
    candidates: picker_state::PickerCandidates,
    cursor: Option<LogicalPosition>,
) {
    let mut s = state.lock().await;
    let key = (client_id, kind);
    let Some(picker) = s.pickers.get_mut(&key) else {
        return; // picker gone (closed/reset)
    };
    if picker.pending_async_load != Some(epoch) {
        return; // superseded by a newer open, or already applied
    }
    picker.pending_async_load = None;
    picker.candidates = candidates;
    let outbound = s.clients.get(&client_id).map(|c| c.outbound.clone());
    let ServerState {
        pickers, matcher, ..
    } = &mut *s;
    let picker = pickers.get_mut(&key).expect("checked above");
    // Rank against the current query — the user may have typed a filter while we resolved.
    picker.rerank(matcher);
    // DocumentSymbols: highlight the symbol the cursor sits in. Picked among the *visible* (ranked)
    // symbols and innermost-first (deepest depth, then latest start), so with the top-level chip on
    // it lands on the enclosing top-level symbol; expanded, on the innermost member. We keep its
    // *rank* (position in `ranked`) so the window can be framed around it — a symbol far down the
    // list (e.g. a field near the bottom of a big file, all levels expanded) would otherwise sit
    // outside the pushed window and never match the client's identity centering.
    let center: Option<(u32, usize)> = match (&picker.candidates, cursor) {
        (picker_state::PickerCandidates::Symbols(syms), Some(pos)) => picker
            .ranked
            .iter()
            .enumerate()
            .filter(|(_, &ci)| syms[ci as usize].contains(pos))
            .max_by_key(|(_, &ci)| {
                let c = &syms[ci as usize];
                (c.depth, c.range_start.line, c.range_start.col)
            })
            .map(|(rank, &ci)| (rank as u32, ci as usize)),
        _ => None,
    };
    // Frame the window: around the centered symbol when there is one (it rides the push as
    // `center_on`, and the client adopts this offset), else clamp a stale offset back into range.
    if let Some(window) = picker.subscribed.as_mut() {
        let total = picker.ranked.len() as u32;
        match center {
            Some((rank, _)) => window.offset = rank.saturating_sub(window.limit / 2),
            None if window.offset >= total => window.offset = total.saturating_sub(window.limit),
            None => {}
        }
    }
    let center_on = center.map(|(_, ci)| Box::new(picker.candidates.make_item(ci, Vec::new())));
    let mut update = picker_state::build_update(picker, matcher);
    if let Some(ref mut u) = update {
        u.ticking = false; // resolve finished
        u.center_on = center_on;
    }
    drop(s);
    if let (Some(sender), Some(params)) = (outbound, update) {
        let _ = sender.send(picker_update_notif(params)).await;
    }
}

/// Resolve the References picker's candidates in the background (an LSP round-trip + file reads,
/// off the lock) and push them into the already-open picker. Detached/fire-and-forget.
pub fn spawn_reference_resolve(
    state: SharedState,
    client_id: ClientId,
    buffer_id: BufferId,
    epoch: u64,
) {
    tokio::spawn(async move {
        let candidates = build_reference_candidates(&state, client_id, buffer_id).await;
        apply_async_candidates(
            &state,
            client_id,
            PickerKind::References,
            epoch,
            picker_state::PickerCandidates::References(candidates),
            None, // references open at the top, no cursor centering
        )
        .await;
    });
}

/// Resolve the DocumentSymbols picker's candidates in the background (one LSP round-trip, off the
/// lock) and push them into the already-open picker. Detached/fire-and-forget.
pub fn spawn_symbol_resolve(
    state: SharedState,
    client_id: ClientId,
    buffer_id: BufferId,
    epoch: u64,
) {
    tokio::spawn(async move {
        let (candidates, cursor) = build_symbol_candidates(&state, client_id, buffer_id).await;
        apply_async_candidates(
            &state,
            client_id,
            PickerKind::DocumentSymbols,
            epoch,
            picker_state::PickerCandidates::Symbols(candidates),
            cursor,
        )
        .await;
    });
}

/// Build the LSP-servers-picker candidates: one per language server whose workspace root falls
/// under `project_roots`, sorted by name then language for a stable order.
fn build_lsp_server_candidates(
    s: &ServerState,
    project_roots: &[std::path::PathBuf],
) -> Vec<picker_state::LspServerCandidate> {
    let mut out: Vec<picker_state::LspServerCandidate> = s
        .lsp
        .status_for_roots(project_roots)
        .into_iter()
        .map(|st| picker_state::LspServerCandidate {
            root_label: lsp_root_label(&st.workspace_root, project_roots),
            name: st.name,
            language: st.language,
            workspace_root: st.workspace_root,
            status: st.status,
            progress: st.progress,
        })
        .collect();
    out.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.language.cmp(&b.language))
    });
    out
}

/// The picker's display label for a server's workspace root: its path relative to the containing
/// project root, or empty when the server is rooted *at* a project root (so single-root projects
/// show no redundant path — only monorepo sub-roots get a disambiguating label).
fn lsp_root_label(workspace_root: &str, project_roots: &[std::path::PathBuf]) -> String {
    let root = std::path::Path::new(workspace_root);
    let base = project_roots
        .iter()
        .filter(|r| root.starts_with(r))
        .max_by_key(|r| r.components().count());
    match base.and_then(|b| root.strip_prefix(b).ok()) {
        Some(rel) if !rel.as_os_str().is_empty() => rel.display().to_string(),
        _ => String::new(),
    }
}

/// Replace a buffer's diagnostics and re-render the viewports showing it, so the new markers appear
/// without an edit. Returns the notifications to send once the state lock is released (the
/// `watcher.rs:183` pattern). A no-op (empty) when the buffer isn't open.
pub fn set_diagnostics_and_refresh(
    s: &mut ServerState,
    buffer_id: BufferId,
    diagnostics: Vec<crate::lsp::diagnostics::BufferDiagnostic>,
) -> PendingPushes {
    s.diagnostics.insert(buffer_id, diagnostics);
    if !s.buffers.contains_key(&buffer_id) {
        return Vec::new();
    }
    let buf = &s.buffers[&buffer_id];
    let revision = buf.revision;
    let diags = buffer_diagnostics(s, buffer_id);
    let counts = diagnostic_counts(diags);
    let mut pushes = Vec::new();
    // One `lsp/diagnostics_changed` (buffer-wide counts) per distinct client viewing the buffer,
    // plus the per-viewport `viewport/lines_changed` re-render (squiggles + gutter).
    let mut counted_clients: std::collections::HashSet<ClientId> = std::collections::HashSet::new();
    for vp in s.viewports.values() {
        if vp.buffer_id != buffer_id {
            continue;
        }
        let Some(sender) = s.clients.get(&vp.client_id).map(|c| c.outbound.clone()) else {
            continue;
        };
        if counted_clients.insert(vp.client_id) {
            pushes.push((sender.clone(), diagnostics_changed_notif(buffer_id, counts)));
        }
        let search = s.searches.get(&(vp.client_id, buffer_id));
        pushes.push((
            sender,
            build_lines_changed_notif(
                buf,
                vp,
                revision,
                search,
                buffer_both_hunks(s, buffer_id),
                diags,
                buffer_git_status(s, buffer_id),
            ),
        ));
    }
    pushes
}

/// Per-severity counts over a buffer's diagnostics, for the status-bar summary.
fn diagnostic_counts(diags: &[crate::lsp::diagnostics::BufferDiagnostic]) -> DiagnosticCounts {
    use aether_protocol::viewport::DiagnosticSeverity::*;
    let mut c = DiagnosticCounts::default();
    for d in diags {
        match d.severity {
            Error => c.errors += 1,
            Warning => c.warnings += 1,
            Information => c.infos += 1,
            Hint => c.hints += 1,
        }
    }
    c
}

fn diagnostics_changed_notif(buffer_id: BufferId, counts: DiagnosticCounts) -> Notification {
    Notification {
        jsonrpc: JsonRpc,
        method: LspDiagnosticsChanged::NAME.into(),
        params: serde_json::to_value(LspDiagnosticsChangedParams { buffer_id, counts })
            .expect("infallible"),
    }
}

/// The per-line footprint of `diags` on `line_idx`: byte ranges clipped to `[0, line_len]`. A
/// diagnostic spanning multiple lines contributes to each line it covers (start line from its
/// column to EOL, middle lines whole, end line up to its column), carrying the full message so the
/// client can show it wherever the cursor sits. Zero-width diagnostics are kept (the client widens
/// them to one cell).
fn diagnostic_spans_on_line(
    diags: &[crate::lsp::diagnostics::BufferDiagnostic],
    line_idx: u32,
    line_len: u32,
) -> Vec<DiagnosticSpan> {
    let mut out = Vec::new();
    for d in diags {
        if line_idx < d.start.line || line_idx > d.end.line {
            continue;
        }
        let s = if line_idx == d.start.line {
            d.start.col
        } else {
            0
        };
        let e = if line_idx == d.end.line {
            d.end.col
        } else {
            line_len
        };
        let s = s.min(line_len);
        let e = e.min(line_len).max(s);
        out.push(DiagnosticSpan {
            start: s,
            end: e,
            severity: d.severity,
            message: d.message.clone(),
        });
    }
    out
}

/// Find the largest `scroll_logical_line` such that the buffer's last visual row sits at the
/// bottom of the viewport. Walks logical lines from the end backward, accumulating their visual
/// row counts under the current wrap settings until we have `viewport_rows` rows. Diff-view
/// phantom rows (`deleted_rows`) count as occupied rows so the bottom of a diff still scrolls into
/// view fully.
fn compute_max_scroll(
    buf: &Buffer,
    viewport_rows: u32,
    cols: u32,
    wrap: aether_protocol::viewport::WrapMode,
    marker_width: u32,
    tab_width: u32,
    deleted_rows: &HashMap<u32, Vec<VirtualRow>>,
) -> u32 {
    let line_count = buf.line_count();
    if viewport_rows == 0 || line_count == 0 {
        return 0;
    }
    let no_wrap = matches!(wrap, aether_protocol::viewport::WrapMode::None);
    if no_wrap && deleted_rows.is_empty() {
        return line_count.saturating_sub(viewport_rows);
    }
    let mut rows_remaining = viewport_rows;
    for line_idx in (0..line_count).rev() {
        let virtual_n = deleted_rows.get(&line_idx).map_or(0, |v| v.len() as u32);
        let real_n = if no_wrap {
            1
        } else {
            let mut text: String = buf.text.line(line_idx as usize).chunks().collect();
            if text.ends_with('\n') {
                text.pop();
            }
            wrap::compute_rows(&text, cols, marker_width, tab_width).len() as u32
        };
        let n = real_n + virtual_n;
        if n >= rows_remaining {
            return line_idx;
        }
        rows_remaining -= n;
    }
    0
}

/// Number of real visual rows for one logical line (1 under no-wrap, else the wrapped count).
fn line_visual_rows(
    buf: &Buffer,
    line_idx: u32,
    no_wrap: bool,
    cols: u32,
    marker_width: u32,
    tab_width: u32,
) -> u32 {
    if no_wrap {
        return 1;
    }
    let mut text: String = buf.text.line(line_idx as usize).chunks().collect();
    if text.ends_with('\n') {
        text.pop();
    }
    wrap::compute_rows(&text, cols, marker_width, tab_width).len() as u32
}

/// `(first_visual_row, total_visual_rows)` for the buffer at this config — the visual row where
/// `first` begins and the buffer's total visual-row height (real + diff phantom rows). O(lines);
/// the no-wrap/no-diff case is O(1).
fn compute_visual_extent(
    buf: &Buffer,
    cols: u32,
    wrap: aether_protocol::viewport::WrapMode,
    marker_width: u32,
    tab_width: u32,
    deleted_rows: &HashMap<u32, Vec<VirtualRow>>,
    first: u32,
) -> (u32, u32) {
    let line_count = buf.line_count();
    let no_wrap = matches!(wrap, aether_protocol::viewport::WrapMode::None);
    if no_wrap && deleted_rows.is_empty() {
        return (first.min(line_count), line_count);
    }
    let mut total = 0u32;
    let mut first_vr = 0u32;
    for i in 0..line_count {
        if i == first {
            first_vr = total;
        }
        let virtual_n = deleted_rows.get(&i).map_or(0, |v| v.len() as u32);
        total = total.saturating_add(
            line_visual_rows(buf, i, no_wrap, cols, marker_width, tab_width) + virtual_n,
        );
    }
    if first >= line_count {
        first_vr = total;
    }
    (first_vr, total)
}

/// Display width (cols) of the widest line in the buffer — sizes a client's native horizontal
/// scroller under no-wrap. O(buffer chars); only called when wrap is off.
fn compute_max_line_width(buf: &Buffer, tab_width: u32) -> u32 {
    let mut max = 0u32;
    for i in 0..buf.line_count() {
        let mut text: String = buf.text.line(i as usize).chunks().collect();
        if text.ends_with('\n') {
            text.pop();
        }
        let mut col = 0u32;
        for c in text.chars() {
            col += wrap::char_display_width(c, col, tab_width);
        }
        max = max.max(col);
    }
    max
}

/// The logical line whose visual-row span contains `target_row` (clamped to the last line).
fn logical_line_at_visual_row(
    buf: &Buffer,
    cols: u32,
    wrap: aether_protocol::viewport::WrapMode,
    marker_width: u32,
    tab_width: u32,
    deleted_rows: &HashMap<u32, Vec<VirtualRow>>,
    target_row: u32,
) -> u32 {
    let line_count = buf.line_count();
    if line_count == 0 {
        return 0;
    }
    let no_wrap = matches!(wrap, aether_protocol::viewport::WrapMode::None);
    if no_wrap && deleted_rows.is_empty() {
        return target_row.min(line_count - 1);
    }
    let mut acc = 0u32;
    for i in 0..line_count {
        let virtual_n = deleted_rows.get(&i).map_or(0, |v| v.len() as u32);
        let n = line_visual_rows(buf, i, no_wrap, cols, marker_width, tab_width) + virtual_n;
        if acc + n > target_row {
            return i;
        }
        acc += n;
    }
    line_count - 1
}

/// Everything that decorates a rendered window beyond the text itself: search highlights, the
/// inline-diff state, diagnostics squiggles, and the buffer's git status. Bundled because every
/// `render_window` caller assembles the same set from `ServerState`.
struct WindowDecorations<'a> {
    search: Option<&'a SearchEntry>,
    diff_view: bool,
    hunks: &'a [crate::git::DiffHunk],
    diagnostics: &'a [crate::lsp::diagnostics::BufferDiagnostic],
    git_status: Option<GitBufferStatus>,
}

fn render_window(
    buf: &Buffer,
    first: u32,
    last_excl: u32,
    geom: wrap::WrapGeometry,
    viewport_rows: u32,
    deco: WindowDecorations<'_>,
) -> Window {
    let WindowDecorations {
        search,
        diff_view,
        hunks,
        diagnostics,
        git_status,
    } = deco;
    let wrap::WrapGeometry {
        wrap,
        cols,
        marker_width,
        tab_width,
    } = geom;
    let mut lines: Vec<LogicalLineRender> = Vec::with_capacity((last_excl - first) as usize);

    // Per-line change markers drive the always-on gutter, so they're computed whenever hunks are
    // known — independent of the diff-view toggle. Phantom "deleted" rows, by contrast, only
    // appear while the diff view is on.
    let markers = diff_markers_by_line(hunks, buf.line_count());
    let deleted_rows = if diff_view {
        deleted_rows_by_anchor(hunks, buf.line_count())
    } else {
        HashMap::new()
    };

    // For highlighting we need the whole source as bytes. Computed once per render rather than
    // per line. Skipped entirely when no syntax is attached.
    let source: Option<String> = buf
        .syntax
        .as_ref()
        .map(|_| buf.text.chunks().collect::<String>());

    for i in first..last_excl {
        let line_slice = buf.text.line(i as usize);
        let mut text: String = line_slice.chunks().collect();
        if text.ends_with('\n') {
            text.pop();
        }

        let highlights = match (&buf.syntax, source.as_deref()) {
            (Some(syntax), Some(source)) => {
                let line_char_start = buf.text.line_to_char(i as usize);
                let line_byte_start = buf.text.char_to_byte(line_char_start);
                let line_byte_end = line_byte_start + text.len();
                crate::syntax::highlights_for_range(
                    syntax.config,
                    &syntax.tree,
                    &syntax.injections,
                    source,
                    line_byte_start,
                    line_byte_end,
                )
            }
            _ => Vec::new(),
        };

        let mut render =
            wrap::render_line(&text, i, cols, wrap, marker_width, tab_width, highlights);
        if let Some(entry) = search {
            render.search_matches = matches_on_line(entry, i, text.len() as u32);
        }
        if let Some(rows) = deleted_rows.get(&i) {
            render.virtual_rows_above = rows.clone();
        }
        if let Some((marker, stage)) = markers.get(&i).copied() {
            render.diff_marker = Some(marker);
            render.diff_stage = stage;
        }
        render.diagnostics = diagnostic_spans_on_line(diagnostics, i, text.len() as u32);
        lines.push(render);
    }
    let (first_visual_row, total_visual_rows) = compute_visual_extent(
        buf,
        cols,
        wrap,
        marker_width,
        tab_width,
        &deleted_rows,
        first,
    );
    let max_line_width = if matches!(wrap, aether_protocol::viewport::WrapMode::None) {
        compute_max_line_width(buf, tab_width)
    } else {
        0
    };
    Window {
        first_logical_line: first,
        last_logical_line_exclusive: last_excl,
        line_count: buf.line_count(),
        max_scroll_logical_line: compute_max_scroll(
            buf,
            viewport_rows,
            cols,
            wrap,
            marker_width,
            tab_width,
            &deleted_rows,
        ),
        total_visual_rows,
        first_visual_row,
        max_line_width,
        git_status,
        lines,
    }
}

/// Per-line byte ranges from `entry.matches` clipped to `[0, line_len)` for `line_idx`. Matches
/// that span multiple lines contribute one range per line they touch.
fn matches_on_line(entry: &SearchEntry, line_idx: u32, line_len: u32) -> Vec<SearchMatchRange> {
    let mut out = Vec::new();
    for (start, end_excl) in &entry.matches {
        if line_idx < start.line || line_idx > end_excl.line {
            continue;
        }
        let s = if line_idx == start.line { start.col } else { 0 };
        let e = if line_idx == end_excl.line {
            end_excl.col
        } else {
            line_len
        };
        let s = s.min(line_len);
        let e = e.min(line_len);
        if s < e {
            out.push(SearchMatchRange { start: s, end: e });
        }
    }
    out
}

// ---- cursor handlers ---------------------------------------------------------------------------

pub async fn cursor_move(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: CursorMoveParams,
) -> Result<CursorState, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    let buf = s
        .buffers
        .get(&params.buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
    let key = (client_id, params.buffer_id);
    let current = s.cursors.get(&key).copied().unwrap_or_default();

    // Visual motions need viewport state (wrap mode + width). Look it up and dispatch to the
    // dedicated resolver; everything else goes through `resolve_motion` which only needs the
    // buffer.
    let virtual_col_in = s.virtual_col.get(&key).copied();
    // `Some(col)` → set virtual col to `col`; `None` → clear it. Only `VisualLine` preserves it.
    let mut new_virtual_col: Option<u32> = None;
    let new_pos = match &params.motion {
        Motion::VisualLine {
            viewport_id,
            direction,
            count,
        } => {
            let vp = s.viewports.get(viewport_id).ok_or_else(|| {
                RpcError::new(
                    aether_protocol::error::ErrorCode::VIEWPORT_NOT_FOUND,
                    format!("unknown viewport_id: {viewport_id}"),
                )
            })?;
            let (pos, target_vcol) = motion::resolve_visual_line(
                buf,
                vp.wrap_geometry(),
                current.position,
                virtual_col_in,
                *direction,
                *count,
            );
            new_virtual_col = Some(target_vcol);
            pos
        }
        Motion::VisualLineStart { viewport_id } => {
            let vp = s.viewports.get(viewport_id).ok_or_else(|| {
                RpcError::new(
                    aether_protocol::error::ErrorCode::VIEWPORT_NOT_FOUND,
                    format!("unknown viewport_id: {viewport_id}"),
                )
            })?;
            motion::resolve_visual_line_start(buf, vp.wrap_geometry(), current.position)
        }
        Motion::VisualLineEnd { viewport_id } => {
            let vp = s.viewports.get(viewport_id).ok_or_else(|| {
                RpcError::new(
                    aether_protocol::error::ErrorCode::VIEWPORT_NOT_FOUND,
                    format!("unknown viewport_id: {viewport_id}"),
                )
            })?;
            motion::resolve_visual_line_end(buf, vp.wrap_geometry(), current.position)
        }
        Motion::LogicalLine {
            direction,
            count,
            preserve_col,
        } => {
            // LogicalLine doesn't reference a viewport, but it does preserve virtual column,
            // which is in display cells — so it needs `tab_width` to be right for tab-bearing
            // lines. Borrow it from any of this client's viewports on this buffer.
            let tab_width = s
                .viewports
                .values()
                .find(|v| v.buffer_id == params.buffer_id && v.client_id == client_id)
                .map(|v| v.tab_width)
                .unwrap_or(4);
            let (pos, target_vcol) = motion::resolve_logical_line(
                buf,
                current.position,
                virtual_col_in,
                *direction,
                *count,
                *preserve_col,
                tab_width,
            );
            new_virtual_col = target_vcol;
            pos
        }
        // Selection-edge motions read the whole selection (anchor + cursor), which
        // `resolve_motion` doesn't see — dispatch to the dedicated resolver.
        Motion::SelectionEdge { edge } => {
            motion::resolve_selection_edge(buf, current.position, current.anchor, *edge)
        }
        _ => motion::resolve_motion(buf, current.position, &params.motion),
    };
    // Extending: keep the current anchor (which may already equal position, i.e. a point).
    // Not extending: collapse to a 1-char point at the new position. The data model always
    // has an anchor, so "no selection" means `anchor == position`.
    let new_anchor = if params.extend_selection {
        current.anchor
    } else {
        new_pos
    };

    let new_state = CursorState {
        position: new_pos,
        anchor: new_anchor,
        match_bracket: None,
        grep_position: None,
    };
    s.cursors.insert(key, new_state);
    s.record_motion(key, current, new_state);
    s.clear_tree_selection_history(client_id, params.buffer_id);
    match new_virtual_col {
        Some(col) => {
            s.virtual_col.insert(key, col);
        }
        None => {
            s.virtual_col.remove(&key);
        }
    }
    let search_update = collect_cursor_search_update(&mut s, client_id, params.buffer_id);
    let response = wrap_for_response(&s, client_id, params.buffer_id, new_state);
    drop(s);
    if let Some((sender, notif)) = search_update {
        let _ = sender.send(notif).await;
    }
    Ok(response)
}

/// Whole-line selection in either direction. The result is always whole lines (anchor at col 0
/// of one line, cursor at the end byte of another); orientation (forward / backward) is whatever
/// the input was.
///
/// Forward grows the *bottom-most* edge of the selection downward; backward grows the *top-most*
/// edge upward. This means edge-extension stays orientation-independent of which end the cursor
/// sits on — useful after `cursor/swap_anchor`. The cursor stays at the end it was already on;
/// the anchor occupies the other end.
///
/// First-press, point-cursor asymmetry: when there's no selection, Forward selects the cursor's
/// line, while Backward (extend or not) selects the line *above* the cursor. That keeps the two
/// bindings distinct on the very first press (otherwise both would just select the current line)
/// and matches a "go up" mental model for Backward. Subsequent presses behave the same as
/// before: Backward + extend then widens upward from there.
pub async fn cursor_select_line(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: CursorSelectLineParams,
) -> Result<CursorState, RpcError> {
    // The repeat loop lives server-side (`3x` = one round-trip).
    let mut last = None;
    for _ in 0..params.count.max(1) {
        last = Some(cursor_select_line_once(state, ctx, &params).await?);
    }
    Ok(last.expect("count.max(1) iterations"))
}

async fn cursor_select_line_once(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: &CursorSelectLineParams,
) -> Result<CursorState, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    let buf = s
        .buffers
        .get(&params.buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
    let key = (client_id, params.buffer_id);
    let current = s.cursors.get(&key).copied().unwrap_or_default();
    let cur = current.position;

    // Top / bottom edges of the current selection, normalized so we can reason about "extend
    // the bottom down" independent of which end the cursor sits on. For a point cursor
    // (anchor == position) both edges land on the cursor.
    let (top_edge, bottom_edge) = if (current.anchor.line, current.anchor.col) < (cur.line, cur.col)
    {
        (current.anchor, cur)
    } else {
        (cur, current.anchor)
    };
    let has_range = !current.is_point();
    let cursor_was_at_top = has_range && cur == top_edge;

    // Advance the relevant edge only when the selection already spans whole lines; otherwise snap
    // it without advancing. A point cursor (anchor == position) on an empty line is trivially
    // whole — its only char is the newline at col 0, so the point already selects the line. So the
    // edge advances past it (plain `x`/`Alt-x` step to the next/previous line rather than getting
    // stuck), and — when extending — it counts as a real range so `Shift-x`/`Alt-Shift-x` grow
    // *over* the empty line instead of jumping past it (the `|| already_whole` in the match).
    // Backward on any point cursor (extend or not) also advances upward, so Alt-x / Alt-Shift-x
    // jump to the line above on the first press (see the doc comment).
    let bottom_len = motion::line_byte_len_excl_newline(buf, bottom_edge.line);
    let already_whole = if has_range {
        top_edge.col == 0 && bottom_edge.col >= bottom_len
    } else {
        bottom_len == 0 && cur.col == 0
    };
    let advance_top_for_backward = already_whole || !has_range;
    let new_top = if advance_top_for_backward && params.direction == Direction::Backward {
        top_edge.line.saturating_sub(1)
    } else {
        top_edge.line
    };
    let new_bottom = if already_whole && params.direction == Direction::Forward {
        bottom_edge.line.saturating_add(1)
    } else {
        bottom_edge.line
    };
    // Extend (grow the span) only when Shift is held *and* there's already a whole-line span to
    // grow from — a real range, or an empty line whose whole-line form is a point. Otherwise
    // collapse to a single line: snap the current line, or step to the next/previous one.
    let (top_line, bottom_line) = match (
        params.extend && (has_range || already_whole),
        params.direction,
    ) {
        (true, _) => (new_top, new_bottom),
        (false, Direction::Forward) => (new_bottom, new_bottom),
        (false, Direction::Backward) => (new_top, new_top),
    };

    let last_line = (buf.text.len_lines() as u32).saturating_sub(1);
    let top_line = top_line.min(last_line);
    let bottom_line = bottom_line.min(last_line);
    let top_pos = LogicalPosition {
        line: top_line,
        col: 0,
    };
    let bottom_pos = LogicalPosition {
        line: bottom_line,
        col: motion::line_byte_len_excl_newline(buf, bottom_line),
    };
    // Cursor stays at the end it occupied (top or bottom). Default to bottom for a fresh
    // selection so the result is forward-oriented.
    let (cursor_pos, anchor_pos) = if cursor_was_at_top {
        (top_pos, bottom_pos)
    } else {
        (bottom_pos, top_pos)
    };
    let new_state = CursorState {
        position: cursor_pos,
        anchor: anchor_pos,
        match_bracket: None,
        grep_position: None,
    };
    s.cursors.insert(key, new_state);
    s.record_motion(key, current, new_state);
    s.virtual_col.remove(&key);
    s.clear_tree_selection_history(client_id, params.buffer_id);
    let search_update = collect_cursor_search_update(&mut s, client_id, params.buffer_id);
    let response = wrap_for_response(&s, client_id, params.buffer_id, new_state);
    drop(s);
    if let Some((sender, notif)) = search_update {
        let _ = sender.send(notif).await;
    }
    Ok(response)
}

pub async fn cursor_swap_anchor(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: CursorSwapAnchorParams,
) -> Result<CursorState, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    if !s.buffers.contains_key(&params.buffer_id) {
        return Err(RpcError::buffer_not_found(params.buffer_id));
    }
    let key = (client_id, params.buffer_id);
    let current = s.cursors.get(&key).copied().unwrap_or_default();
    // Swap anchor and position. For a point cursor (anchor == position) this is a no-op.
    let new_state = CursorState {
        position: current.anchor,
        anchor: current.position,
        match_bracket: None,
        grep_position: None,
    };
    s.cursors.insert(key, new_state);
    s.record_motion(key, current, new_state);
    s.virtual_col.remove(&key);
    s.clear_tree_selection_history(client_id, params.buffer_id);
    let search_update = collect_cursor_search_update(&mut s, client_id, params.buffer_id);
    let response = wrap_for_response(&s, client_id, params.buffer_id, new_state);
    drop(s);
    if let Some((sender, notif)) = search_update {
        let _ = sender.send(notif).await;
    }
    Ok(response)
}

pub async fn cursor_set(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: CursorSetParams,
) -> Result<CursorState, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    let buf = s
        .buffers
        .get(&params.buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
    let key = (client_id, params.buffer_id);
    let current = s.cursors.get(&key).copied().unwrap_or_default();
    let position = motion::clamp_position(buf, params.position);
    let anchor = motion::clamp_position(buf, params.anchor);
    let (position, anchor) = motion::snap_selection(buf, position, anchor, params.granularity);
    let result = CursorState {
        position,
        anchor,
        match_bracket: None,
        grep_position: None,
    };
    s.cursors.insert(key, result);
    s.record_motion(key, current, result);
    s.virtual_col.remove(&key);
    s.clear_tree_selection_history(client_id, params.buffer_id);
    let search_update = collect_cursor_search_update(&mut s, client_id, params.buffer_id);
    let response = wrap_for_response(&s, client_id, params.buffer_id, result);
    drop(s);
    if let Some((sender, notif)) = search_update {
        let _ = sender.send(notif).await;
    }
    Ok(response)
}

/// Rewind one step on this client's per-buffer motion history. Independent of `input/undo`.
pub async fn cursor_undo(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: CursorUndoParams,
) -> Result<CursorUndoResult, RpcError> {
    // The repeat loop lives server-side, stopping once the history is exhausted (the
    // `applied: false` result is returned so the client still learns the final state).
    let mut last = None;
    for _ in 0..params.count.max(1) {
        let r = cursor_undo_once(state, ctx, &params).await?;
        let applied = r.applied;
        last = Some(r);
        if !applied {
            break;
        }
    }
    Ok(last.expect("count.max(1) iterations"))
}

async fn cursor_undo_once(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: &CursorUndoParams,
) -> Result<CursorUndoResult, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    if !s.buffers.contains_key(&params.buffer_id) {
        return Err(RpcError::buffer_not_found(params.buffer_id));
    }
    let key = (client_id, params.buffer_id);
    let current = s.cursors.get(&key).copied().unwrap_or_default();

    let history = s.motion_history.entry(key).or_default();
    if history.undo.is_empty() {
        return Ok(CursorUndoResult {
            applied: false,
            cursor: current,
        });
    }
    let prev = history.undo.pop_back().expect("just checked non-empty");
    history.redo.push(current);
    while history.redo.len() > MOTION_HISTORY_CAP {
        history.redo.remove(0);
    }

    s.cursors.insert(key, prev);
    s.virtual_col.remove(&key);
    s.clear_tree_selection_history(client_id, params.buffer_id);
    let search_update = collect_cursor_search_update(&mut s, client_id, params.buffer_id);
    let prev = wrap_for_response(&s, client_id, params.buffer_id, prev);
    drop(s);
    if let Some((sender, notif)) = search_update {
        let _ = sender.send(notif).await;
    }
    Ok(CursorUndoResult {
        applied: true,
        cursor: prev,
    })
}

pub async fn cursor_redo(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: CursorUndoParams,
) -> Result<CursorUndoResult, RpcError> {
    // The repeat loop lives server-side, stopping once the history is exhausted (the
    // `applied: false` result is returned so the client still learns the final state).
    let mut last = None;
    for _ in 0..params.count.max(1) {
        let r = cursor_redo_once(state, ctx, &params).await?;
        let applied = r.applied;
        last = Some(r);
        if !applied {
            break;
        }
    }
    Ok(last.expect("count.max(1) iterations"))
}

async fn cursor_redo_once(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: &CursorUndoParams,
) -> Result<CursorUndoResult, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    if !s.buffers.contains_key(&params.buffer_id) {
        return Err(RpcError::buffer_not_found(params.buffer_id));
    }
    let key = (client_id, params.buffer_id);
    let current = s.cursors.get(&key).copied().unwrap_or_default();

    let history = s.motion_history.entry(key).or_default();
    if history.redo.is_empty() {
        return Ok(CursorUndoResult {
            applied: false,
            cursor: current,
        });
    }
    let next = history.redo.pop().expect("just checked non-empty");
    history.undo.push_back(current);
    while history.undo.len() > MOTION_HISTORY_CAP {
        history.undo.pop_front();
    }

    s.cursors.insert(key, next);
    s.virtual_col.remove(&key);
    s.clear_tree_selection_history(client_id, params.buffer_id);
    let search_update = collect_cursor_search_update(&mut s, client_id, params.buffer_id);
    let next = wrap_for_response(&s, client_id, params.buffer_id, next);
    drop(s);
    if let Some((sender, notif)) = search_update {
        let _ = sender.send(notif).await;
    }
    Ok(CursorUndoResult {
        applied: true,
        cursor: next,
    })
}

// ---- cursor/expand and cursor/contract ---------------------------------------------------------

pub async fn cursor_expand(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: CursorBufferOnlyParams,
) -> Result<CursorState, RpcError> {
    // Repeat server-side, stopping once the cursor stops changing (top of the tree /
    // single node — repeated presses are a no-op, not an error).
    let mut last: Option<CursorState> = None;
    for _ in 0..params.count.max(1) {
        let r = cursor_expand_once(state, ctx, &params).await?;
        if last == Some(r) {
            break;
        }
        last = Some(r);
    }
    Ok(last.expect("count.max(1) iterations"))
}

async fn cursor_expand_once(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: &CursorBufferOnlyParams,
) -> Result<CursorState, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    let buf = s
        .buffers
        .get(&params.buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
    let key = (client_id, params.buffer_id);
    let current = s.cursors.get(&key).copied().unwrap_or_default();

    let Some(syntax) = buf.syntax.as_ref() else {
        return Ok(current);
    };

    // Compute the current selection's byte range. For collapsed cursors, treat as the single
    // char under the cursor (one-byte minimum so descendant_for_byte_range can find it).
    let (sel_start_char, sel_end_char_excl) = current_selection_char_range(buf, &current);
    let total_bytes = buf.text.len_bytes();
    let start_byte = buf.text.char_to_byte(sel_start_char).min(total_bytes);
    let end_byte_excl = buf.text.char_to_byte(sel_end_char_excl).min(total_bytes);

    // Smallest descendant containing the byte range, then walk up while the node exactly equals
    // our selection — that gives the smallest *strictly larger* enclosing node.
    let root = syntax.tree.root_node();
    let mut node = root
        .descendant_for_byte_range(start_byte, end_byte_excl)
        .unwrap_or(root);
    while node.start_byte() == start_byte && node.end_byte() == end_byte_excl {
        match node.parent() {
            Some(p) => node = p,
            None => return Ok(current), // already at the root
        }
    }

    let new_start_char = buf.text.byte_to_char(node.start_byte());
    let new_end_char_excl = buf
        .text
        .byte_to_char(node.end_byte())
        .max(new_start_char + 1);
    let new_last_char = new_end_char_excl.saturating_sub(1).max(new_start_char);
    let anchor = motion::char_to_pos(buf, new_start_char);
    let position = motion::char_to_pos(buf, new_last_char);
    let new_cursor = CursorState {
        position,
        anchor,
        match_bracket: None,
        grep_position: None,
    };

    s.cursors.insert(key, new_cursor);
    s.record_motion(key, current, new_cursor);
    s.virtual_col.remove(&key);
    s.tree_selection_history
        .entry(key)
        .or_default()
        .push(current);
    let search_update = collect_cursor_search_update(&mut s, client_id, params.buffer_id);
    let new_cursor = wrap_for_response(&s, client_id, params.buffer_id, new_cursor);
    drop(s);
    if let Some((sender, notif)) = search_update {
        let _ = sender.send(notif).await;
    }
    Ok(new_cursor)
}

pub async fn cursor_contract(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: CursorBufferOnlyParams,
) -> Result<CursorState, RpcError> {
    // Repeat server-side, stopping once the cursor stops changing (top of the tree /
    // single node — repeated presses are a no-op, not an error).
    let mut last: Option<CursorState> = None;
    for _ in 0..params.count.max(1) {
        let r = cursor_contract_once(state, ctx, &params).await?;
        if last == Some(r) {
            break;
        }
        last = Some(r);
    }
    Ok(last.expect("count.max(1) iterations"))
}

async fn cursor_contract_once(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: &CursorBufferOnlyParams,
) -> Result<CursorState, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    if !s.buffers.contains_key(&params.buffer_id) {
        return Err(RpcError::buffer_not_found(params.buffer_id));
    }
    let key = (client_id, params.buffer_id);
    let prev = s
        .tree_selection_history
        .get_mut(&key)
        .and_then(|stack| stack.pop());
    let Some(prev) = prev else {
        // Nothing to contract back to.
        let cur = s.cursors.get(&key).copied().unwrap_or_default();
        return Ok(wrap_for_response(&s, client_id, params.buffer_id, cur));
    };
    let current = s.cursors.get(&key).copied().unwrap_or_default();
    s.cursors.insert(key, prev);
    s.record_motion(key, current, prev);
    s.virtual_col.remove(&key);
    let search_update = collect_cursor_search_update(&mut s, client_id, params.buffer_id);
    let prev = wrap_for_response(&s, client_id, params.buffer_id, prev);
    drop(s);
    if let Some((sender, notif)) = search_update {
        let _ = sender.send(notif).await;
    }
    Ok(prev)
}

/// Char range `[start, end_excl)` covered by the cursor's current selection. Collapsed cursors
/// (no anchor) yield a 1-char range so byte conversion produces a non-empty span.
fn current_selection_char_range(buf: &Buffer, cursor: &CursorState) -> (usize, usize) {
    let (lo_pos, hi_pos) = motion::ordered(cursor.position, cursor.anchor);
    let total = buf.text.len_chars();
    let lo = motion::pos_to_char(buf, lo_pos).min(total);
    let hi_inclusive = motion::pos_to_char(buf, hi_pos).min(total);
    (
        lo,
        (hi_inclusive + 1).min(total).max(lo + 1).min(total.max(lo)),
    )
}

/// Whether the single chars immediately outside each end of the selection form a known delimiter
/// pair — the precondition unsurround strips on. `current_selection_char_range` gives the
/// selection as `[sc, ec)`, so the hugging chars are at `sc - 1` and `ec`; both must exist.
fn has_enclosing_pair(buf: &Buffer, cursor: &CursorState) -> bool {
    let (sc, ec) = current_selection_char_range(buf, cursor);
    if sc < 1 || ec >= buf.text.len_chars() {
        return false;
    }
    surround::matching_pair(buf.text.char(sc - 1), buf.text.char(ec))
}

/// Char range `[start, end)` of a line's content, excluding the trailing newline — the span a
/// line-scoped surround/unsurround wraps or strips.
fn line_content_char_range(buf: &Buffer, line: usize) -> (usize, usize) {
    let start = buf.text.line_to_char(line);
    let line_slice = buf.text.line(line);
    let len_chars = line_slice.len_chars();
    let has_trailing_nl = len_chars > 0 && line_slice.char(len_chars - 1) == '\n';
    let content_chars = if has_trailing_nl {
        len_chars - 1
    } else {
        len_chars
    };
    (start, start + content_chars)
}

/// Whether the cursor line's content begins and ends with a known delimiter pair — the precondition
/// line-scoped unsurround strips on. Needs at least two content chars (the two delimiters).
fn line_has_enclosing_pair(buf: &Buffer, line: usize) -> bool {
    let (sc, ec) = line_content_char_range(buf, line);
    if ec < sc + 2 {
        return false;
    }
    surround::matching_pair(buf.text.char(sc), buf.text.char(ec - 1))
}

/// Echo the buffer's current revision and the client's cursor without editing — the `EditResult`
/// an edit RPC returns when it resolves to a no-op (unknown surround delimiter, no enclosing pair).
async fn current_edit_result(
    state: &SharedState,
    client_id: ClientId,
    buffer_id: BufferId,
) -> Result<EditResult, RpcError> {
    let s = state.lock().await;
    let buf = s
        .buffers
        .get(&buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let revision = buf.revision;
    let cursor = s
        .cursors
        .get(&(client_id, buffer_id))
        .copied()
        .unwrap_or_default();
    let cursor = wrap_for_response(&s, client_id, buffer_id, cursor);
    Ok(EditResult { revision, cursor })
}

// ---- input handlers ----------------------------------------------------------------------------

pub async fn input_text(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: InputTextParams,
) -> Result<EditResult, RpcError> {
    let client_id = ctx.client_id;
    // Composite pre-step (docs/protocol-composites.md, D): collapse to the requested
    // selection edge before inserting — the same state changes as a `cursor/set`.
    if let Some(edge) = params.at {
        let mut s = state.lock().await;
        let buf = s
            .buffers
            .get(&params.buffer_id)
            .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
        let key = (client_id, params.buffer_id);
        let current = s.cursors.get(&key).copied().unwrap_or_default();
        let pos = motion::resolve_selection_edge(buf, current.position, current.anchor, edge);
        let collapsed = CursorState {
            position: pos,
            anchor: pos,
            match_bracket: None,
            grep_position: None,
        };
        s.cursors.insert(key, collapsed);
        s.record_motion(key, current, collapsed);
        s.virtual_col.remove(&key);
        s.clear_tree_selection_history(client_id, params.buffer_id);
    }
    apply_edit(
        state,
        client_id,
        params.buffer_id,
        EditKind::ReplaceWith {
            text: params.text,
            select_pasted: params.select_pasted,
        },
    )
    .await
}

pub async fn input_delete(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: CountedEditParams,
) -> Result<EditResult, RpcError> {
    let client_id = ctx.client_id;
    let mut last = None;
    for _ in 0..params.count.max(1) {
        last =
            Some(apply_edit(state, client_id, params.buffer_id, EditKind::DeleteSelection).await?);
    }
    Ok(last.expect("count.max(1) iterations"))
}

pub async fn input_backspace(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: BufferOnlyParams,
) -> Result<EditResult, RpcError> {
    let client_id = ctx.client_id;
    apply_edit(state, client_id, params.buffer_id, EditKind::Backspace).await
}

pub async fn input_delete_line(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: BufferOnlyParams,
) -> Result<EditResult, RpcError> {
    let client_id = ctx.client_id;
    apply_edit(state, client_id, params.buffer_id, EditKind::DeleteLine).await
}

pub async fn input_change_line(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: BufferOnlyParams,
) -> Result<EditResult, RpcError> {
    let client_id = ctx.client_id;
    apply_edit(state, client_id, params.buffer_id, EditKind::ChangeLine).await
}

pub async fn input_replace_line(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: aether_protocol::input::InputReplaceLineParams,
) -> Result<EditResult, RpcError> {
    let client_id = ctx.client_id;
    apply_edit(
        state,
        client_id,
        params.buffer_id,
        EditKind::ReplaceLine { text: params.text },
    )
    .await
}

pub async fn input_surround(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: InputSurroundParams,
) -> Result<EditResult, RpcError> {
    let client_id = ctx.client_id;
    // An unrecognized delimiter key is a no-op — echo the current state unchanged.
    let Some((open, close)) = surround::open_close(params.delimiter) else {
        return current_edit_result(state, client_id, params.buffer_id).await;
    };
    let line = matches!(params.target, SurroundTarget::Line);
    apply_edit(
        state,
        client_id,
        params.buffer_id,
        EditKind::Surround { open, close, line },
    )
    .await
}

pub async fn input_unsurround(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: InputUnsurroundParams,
) -> Result<EditResult, RpcError> {
    let client_id = ctx.client_id;
    let line = matches!(params.target, SurroundTarget::Line);
    // No-op unless a known delimiter pair hugs the target. Checked up front so we never push a
    // no-op undo entry through `apply_edit`.
    {
        let s = state.lock().await;
        let buf = s
            .buffers
            .get(&params.buffer_id)
            .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
        let cursor = s
            .cursors
            .get(&(client_id, params.buffer_id))
            .copied()
            .unwrap_or_default();
        let has_pair = if line {
            line_has_enclosing_pair(buf, cursor.position.line as usize)
        } else {
            has_enclosing_pair(buf, &cursor)
        };
        if !has_pair {
            let revision = buf.revision;
            let cursor = wrap_for_response(&s, client_id, params.buffer_id, cursor);
            return Ok(EditResult { revision, cursor });
        }
    }
    apply_edit(
        state,
        client_id,
        params.buffer_id,
        EditKind::Unsurround { line },
    )
    .await
}

pub async fn input_undo(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: CountedEditParams,
) -> Result<UndoResult, RpcError> {
    undo_redo_counted(state, ctx, params, UndoDirection::Undo).await
}

pub async fn input_redo(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: CountedEditParams,
) -> Result<UndoResult, RpcError> {
    undo_redo_counted(state, ctx, params, UndoDirection::Redo).await
}

/// `3u`: step the undo/redo stack `count` times, stopping early once it's exhausted (the
/// `applied: false` result is returned so the client still learns the final state).
async fn undo_redo_counted(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: CountedEditParams,
    direction: UndoDirection,
) -> Result<UndoResult, RpcError> {
    let mut last = None;
    for _ in 0..params.count.max(1) {
        let r = apply_undo_or_redo(state, ctx, params.buffer_id, direction).await?;
        let applied = r.applied;
        last = Some(r);
        if !applied {
            break;
        }
    }
    Ok(last.expect("count.max(1) iterations"))
}

pub async fn input_indent(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: CountedEditParams,
) -> Result<EditResult, RpcError> {
    let mut last = None;
    for _ in 0..params.count.max(1) {
        last = Some(apply_indent_or_dedent(state, ctx, params.buffer_id, IndentKind::Indent).await?);
    }
    Ok(last.expect("count.max(1) iterations"))
}

/// `input/open_line` — the open-line chains (cursor-park, edit, land) composed server-side
/// from the same handlers the clients used to call in sequence, so undo grouping, pushes,
/// and cursor stamping are identical (docs/protocol-composites.md, E).
pub async fn input_open_line(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: InputOpenLineParams,
) -> Result<EditResult, RpcError> {
    let line = {
        let s = state.lock().await;
        if !s.buffers.contains_key(&params.buffer_id) {
            return Err(RpcError::buffer_not_found(params.buffer_id));
        }
        s.cursors
            .get(&(ctx.client_id, params.buffer_id))
            .copied()
            .unwrap_or_default()
            .position
            .line
    };
    let park = |col: u32| {
        let target = LogicalPosition { line, col };
        CursorSetParams {
            buffer_id: params.buffer_id,
            position: target,
            anchor: target,
            granularity: Granularity::Char,
        }
    };
    match params.side {
        LineSide::Below => {
            // Park at the line's end, then newline + smart indent; stay there.
            cursor_set(state, ctx, park(u32::MAX)).await?;
            input_newline_and_indent(
                state,
                ctx,
                BufferOnlyParams {
                    buffer_id: params.buffer_id,
                },
            )
            .await
        }
        LineSide::Above => {
            // Park at col 0, insert "\n" (pushes the line down), step back up.
            cursor_set(state, ctx, park(0)).await?;
            let r = input_text(
                state,
                ctx,
                InputTextParams {
                    buffer_id: params.buffer_id,
                    text: "\n".into(),
                    select_pasted: false,
                    at: None,
                },
            )
            .await?;
            let cursor = cursor_move(
                state,
                ctx,
                CursorMoveParams {
                    buffer_id: params.buffer_id,
                    motion: Motion::LogicalLine {
                        direction: Direction::Backward,
                        count: 1,
                        preserve_col: false,
                    },
                    extend_selection: false,
                },
            )
            .await?;
            Ok(EditResult {
                revision: r.revision,
                cursor,
            })
        }
    }
}

pub async fn input_newline_and_indent(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: BufferOnlyParams,
) -> Result<EditResult, RpcError> {
    let client_id = ctx.client_id;
    let indent = {
        let s = state.lock().await;
        let buf = s
            .buffers
            .get(&params.buffer_id)
            .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
        let cursor = s
            .cursors
            .get(&(client_id, params.buffer_id))
            .copied()
            .unwrap_or_default();
        compute_smart_indent(buf, cursor.position)
    };
    let mut text = String::with_capacity(indent.len() + 1);
    text.push('\n');
    text.push_str(&indent);
    apply_edit(
        state,
        client_id,
        params.buffer_id,
        EditKind::ReplaceWith {
            text,
            select_pasted: false,
        },
    )
    .await
}

/// Choose the indent to emit after `\n`. When the buffer's language has an `indents.scm`
/// query (vendored from Helix), runs the tree-sitter indent engine and multiplies its level
/// count by `INDENT_UNIT`. Otherwise falls back to copying the previous non-empty line's
/// leading whitespace.
///
/// The engine alone misses the very common "user just typed `fn foo() {` and pressed Enter"
/// case: the parser hasn't seen a closing brace yet, so no `block` node exists and no
/// `@indent` fires. We patch this with a small heuristic floor — `prev_line_levels +
/// opener_bonus` — taken as `max` with the engine's answer. For complete code the engine
/// already produces the right number, so the heuristic is a no-op; for incomplete code it
/// recovers the level the parser couldn't.
fn compute_smart_indent(buf: &Buffer, cursor_pos: LogicalPosition) -> String {
    let unit = buf.indent_style.unit();

    let line_idx = cursor_pos.line as usize;
    if line_idx >= buf.text.len_lines() {
        return String::new();
    }

    let Some(syntax) = buf.syntax.as_ref() else {
        return previous_line_indent(buf, line_idx);
    };
    let Some(iq) = syntax.config.indent_query.as_ref() else {
        return previous_line_indent(buf, line_idx);
    };

    let line_slice = buf.text.line(line_idx);
    let line_byte_len = {
        let n = line_slice.len_bytes();
        if n > 0 && line_slice.byte(n - 1) == b'\n' {
            n - 1
        } else {
            n
        }
    };
    let col = (cursor_pos.col as usize).min(line_byte_len);
    let line_start_char = buf.text.line_to_char(line_idx);
    let line_start_byte = buf.text.char_to_byte(line_start_char);
    let cursor_byte = line_start_byte + col;
    let source: String = buf.text.chunks().collect();

    let target_levels = crate::indent::compute_indent_levels(
        iq,
        &syntax.tree,
        source.as_bytes(),
        cursor_byte,
        line_idx + 1,
    );

    // Engine-only is enough when it returned anything non-zero — the parse covered the
    // construct and the @indent / @outdent rules already account for it. We only step in
    // with the opener heuristic when the engine reported zero levels *and* the user just
    // typed a code-context opener — that's the "incomplete parse" signature.
    if target_levels > 0 {
        return unit.repeat(target_levels as usize);
    }
    let line_text: String = line_slice.chunks().collect();
    let line_content = line_text.strip_suffix('\n').unwrap_or(&line_text);
    let prefix = &line_content[..col];
    let trimmed = prefix.trim_end_matches([' ', '\t']);
    let mut opener_bonus = match trimmed.as_bytes().last() {
        Some(b'{') | Some(b'(') | Some(b'[') => 1,
        _ => 0,
    };
    if opener_bonus > 0 {
        let opener_byte = line_start_byte + trimmed.len() - 1;
        let node = syntax
            .tree
            .root_node()
            .descendant_for_byte_range(opener_byte, opener_byte + 1);
        if let Some(n) = node {
            let kind = n.kind();
            if kind.contains("string") || kind.contains("comment") || kind.contains("char") {
                opener_bonus = 0;
            }
        }
    }
    unit.repeat(opener_bonus as usize)
}

/// Fallback indent for buffers without an indent query: copy the leading whitespace of the
/// nearest preceding non-blank line. If no such line exists, return empty.
fn previous_line_indent(buf: &Buffer, line_idx: usize) -> String {
    let mut i = line_idx;
    loop {
        let line: String = buf.text.line(i).chunks().collect();
        let content = line.strip_suffix('\n').unwrap_or(&line);
        if !content.trim().is_empty() {
            return content.chars().take_while(|c| c.is_whitespace()).collect();
        }
        if i == 0 {
            return String::new();
        }
        i -= 1;
    }
}

pub async fn input_toggle_comment(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: BufferOnlyParams,
) -> Result<EditResult, RpcError> {
    apply_toggle_comment(state, ctx, params.buffer_id).await
}

/// Toggle comment status on the cursor/selection.
///
/// Decision tree (closest to what users expect from `Ctrl-/` in modern editors):
///   1. If the language has a *line* token and every non-blank line in the affected range
///      already starts with it → strip it.
///   2. Else if the language has *block* tokens and the cursor sits inside a block-comment
///      node (via tree-sitter) or the selection exactly wraps a `start…end` span → strip
///      them.
///   3. Else if the selection is *partial-line* and the language has block tokens → wrap.
///   4. Else if the language has a line token → add the line prefix on each line, aligned to
///      the smallest indent so prefixes line up.
///   5. Else if the language has block tokens → wrap (for languages with no line form).
///   6. Else → no-op.
async fn apply_toggle_comment(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    buffer_id: BufferId,
) -> Result<EditResult, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    let buf = s
        .buffers
        .get(&buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let cursor = s
        .cursors
        .get(&(client_id, buffer_id))
        .copied()
        .unwrap_or_default();

    let (line_tok, block_tok) = buf
        .syntax
        .as_ref()
        .map(|sy| (sy.config.line_comment, sy.config.block_comment))
        .unwrap_or((None, None));
    if line_tok.is_none() && block_tok.is_none() {
        let revision = buf.revision;
        let response = wrap_for_response(&s, client_id, buffer_id, cursor);
        return Ok(EditResult {
            revision,
            cursor: response,
        });
    }

    // Selection / line range.
    let (start, end) = motion::ordered(cursor.position, cursor.anchor);
    let (a, b) = (start.line, end.line);
    let is_partial = is_partial_line_selection(buf, &cursor);

    // Phase 1: decide the action.
    let line_strings: Vec<String> = (a..=b)
        .map(|i| buf.text.line(i as usize).chunks().collect())
        .collect();
    let line_classify = classify_line_range(&line_strings, line_tok);

    let sel_block_unwrap = block_tok.and_then(|(open, close)| {
        // Primary detector: tree-sitter `comment` ancestor containing the cursor. Handles the
        // natural "wrap, then re-toggle to unwrap" gesture where the selection sits on the
        // inner content rather than around the wrappers.
        if let Some(syntax) = buf.syntax.as_ref() {
            let cursor_byte = buf
                .text
                .char_to_byte(motion::pos_to_char(buf, cursor.position));
            let source: String = buf.text.chunks().collect();
            if let Some((s, e)) = find_enclosing_block_comment(
                &syntax.tree,
                source.as_bytes(),
                cursor_byte,
                open,
                close,
            ) {
                let span = source[s..e].to_string();
                return Some((s, e, span, open, close));
            }
        }
        // Fallback: the selection's text *exactly* equals a wrapped span. Catches incomplete
        // parses where tree-sitter doesn't recognise the comment yet (e.g. the user just
        // typed an opener without a closer).
        let (start_pos, end_pos) = ordered_selection_or_cursor_line(&cursor);
        let start_char = motion::pos_to_char(buf, start_pos);
        let end_char_excl = motion::pos_to_char(buf, end_pos)
            .saturating_add(1)
            .min(buf.text.len_chars());
        let span: String = buf.text.slice(start_char..end_char_excl).chunks().collect();
        if span.starts_with(open) && span.ends_with(close) && span.len() >= open.len() + close.len()
        {
            Some((start_char, end_char_excl, span, open, close))
        } else {
            None
        }
    });

    enum Plan {
        Noop,
        LineUncomment {
            prefix: &'static str,
        },
        LineComment {
            prefix: &'static str,
            min_indent: usize,
        },
        BlockUnwrap {
            start_char: usize,
            end_char_excl: usize,
            span: String,
            open: &'static str,
            close: &'static str,
        },
        BlockWrap {
            start_char: usize,
            end_char_excl: usize,
            open: &'static str,
            close: &'static str,
        },
    }

    let plan = if let (Some(prefix), Some(c)) = (line_tok, &line_classify) {
        if c.all_commented {
            Plan::LineUncomment { prefix }
        } else if let Some((sc, ec, span, open, close)) = sel_block_unwrap {
            Plan::BlockUnwrap {
                start_char: sc,
                end_char_excl: ec,
                span,
                open,
                close,
            }
        } else if let (true, Some((open, close))) = (is_partial, block_tok) {
            let (start_pos, end_pos) = ordered_selection_or_cursor_line(&cursor);
            let sc = motion::pos_to_char(buf, start_pos);
            let ec = motion::pos_to_char(buf, end_pos)
                .saturating_add(1)
                .min(buf.text.len_chars());
            Plan::BlockWrap {
                start_char: sc,
                end_char_excl: ec,
                open,
                close,
            }
        } else if c.any_nonblank {
            Plan::LineComment {
                prefix,
                min_indent: c.min_indent,
            }
        } else {
            Plan::Noop
        }
    } else if let Some((sc, ec, span, open, close)) = sel_block_unwrap {
        Plan::BlockUnwrap {
            start_char: sc,
            end_char_excl: ec,
            span,
            open,
            close,
        }
    } else if let Some((open, close)) = block_tok {
        // No line tokens at all (markdown, html, css): everything routes to block.
        let endpoints = if !cursor.is_point() {
            Some(ordered_selection_or_cursor_line(&cursor))
        } else {
            // Cursor-only: wrap the current line's content. Skip empty lines entirely —
            // otherwise the wrap would swallow the line's `\n` and merge it with the next.
            current_line_content_endpoints(buf, cursor.position.line)
        };
        match endpoints {
            None => Plan::Noop,
            Some((start_pos, end_pos)) => {
                let sc = motion::pos_to_char(buf, start_pos);
                let ec = motion::pos_to_char(buf, end_pos)
                    .saturating_add(1)
                    .min(buf.text.len_chars());
                if sc == ec {
                    Plan::Noop
                } else {
                    Plan::BlockWrap {
                        start_char: sc,
                        end_char_excl: ec,
                        open,
                        close,
                    }
                }
            }
        }
    } else {
        Plan::Noop
    };

    // Phase 2: materialize the edit. Each variant produces (edit_start_char, edit_end_char,
    // replacement_text, new_cursor).
    let edit: Option<(usize, usize, String, CursorState, u32, u32)> = match plan {
        Plan::Noop => None,
        Plan::LineUncomment { prefix } => {
            let (start_char, end_char) = line_edit_char_range(buf, a, b);
            let (text, shifts, insert_cols) = build_line_uncomment(&line_strings, a, prefix);
            let nc = shift_cursor_by_line_map(cursor, a, b, &shifts, &insert_cols);
            Some((start_char, end_char, text, nc, a, b))
        }
        Plan::LineComment { prefix, min_indent } => {
            let (start_char, end_char) = line_edit_char_range(buf, a, b);
            let (text, shifts, insert_cols) =
                build_line_comment(&line_strings, a, prefix, min_indent);
            let nc = shift_cursor_by_line_map(cursor, a, b, &shifts, &insert_cols);
            Some((start_char, end_char, text, nc, a, b))
        }
        Plan::BlockUnwrap {
            start_char,
            end_char_excl,
            span,
            open,
            close,
        } => {
            // Strip `open` + optional inner space at the front, optional inner space + `close`
            // at the back. Replace the wrapped span with the inner content; re-select that
            // content.
            let inner_start = open.len();
            let inner_end = span.len() - close.len();
            let mut inner = &span[inner_start..inner_end];
            if inner.starts_with(' ') {
                inner = &inner[1..];
            }
            if inner.ends_with(' ') {
                inner = &inner[..inner.len() - 1];
            }
            let new_text = inner.to_string();
            let start_pos = motion::char_to_pos(buf, start_char);
            // Compute the post-edit position of inner's last byte directly. Walk to the last
            // byte and ask "how many newlines came strictly before it, and where was the last
            // one?". When inner *ends* with `\n` the cursor lands on that `\n` itself (which
            // belongs to the previous line) — naively splitting on `\n` would wrongly put the
            // cursor at col 0 of an empty trailing line.
            let new_position = if inner.is_empty() {
                start_pos
            } else {
                let last_byte_idx = inner.len() - 1;
                let prefix = &inner[..last_byte_idx];
                let newlines_before = prefix.matches('\n').count() as u32;
                match prefix.rfind('\n') {
                    Some(last_nl) => aether_protocol::LogicalPosition {
                        line: start_pos.line + newlines_before,
                        col: (last_byte_idx - last_nl - 1) as u32,
                    },
                    None => aether_protocol::LogicalPosition {
                        line: start_pos.line,
                        col: start_pos.col + last_byte_idx as u32,
                    },
                }
            };
            let nc = if start_pos == new_position {
                CursorState {
                    position: new_position,
                    anchor: new_position,
                    match_bracket: None,
                    grep_position: None,
                }
            } else {
                CursorState {
                    position: new_position,
                    anchor: start_pos,
                    match_bracket: None,
                    grep_position: None,
                }
            };
            let last_line = motion::char_to_pos(buf, end_char_excl.saturating_sub(1)).line;
            Some((
                start_char,
                end_char_excl,
                new_text,
                nc,
                a.min(last_line),
                b.max(last_line),
            ))
        }
        Plan::BlockWrap {
            start_char,
            end_char_excl,
            open,
            close,
        } => {
            let selected: String = buf.text.slice(start_char..end_char_excl).chunks().collect();
            let new_text = format!("{open} {selected} {close}");
            // Compute new selection endpoints in (line, col) directly — `char_to_pos` on the
            // pre-edit buffer is wrong for post-edit char indices once the wrap spans lines.
            // Discriminate by whether the *selected text* contains a newline, not by whether
            // start_pos.line == end_pos.line: a selection ending exactly on the `\n` of its
            // line counts as single-line in (line, col) terms but produces multi-line output.
            let start_pos = motion::char_to_pos(buf, start_char);
            let end_pos = motion::char_to_pos(buf, end_char_excl.saturating_sub(1));
            let newlines = selected.matches('\n').count() as u32;
            let new_position = if newlines == 0 {
                aether_protocol::LogicalPosition {
                    line: end_pos.line,
                    col: end_pos.col + open.len() as u32 + close.len() as u32 + 2,
                }
            } else {
                // The wrap's last line consists of whatever followed the last newline in the
                // selected text, plus the inserted ` close`.
                let last_nl_byte = selected.rfind('\n').unwrap();
                let bytes_after_last_nl = (selected.len() - last_nl_byte - 1) as u32;
                let last_line_bytes = bytes_after_last_nl + 1 + close.len() as u32;
                aether_protocol::LogicalPosition {
                    line: start_pos.line + newlines,
                    col: last_line_bytes.saturating_sub(1),
                }
            };
            let nc = if start_pos == new_position {
                CursorState {
                    position: new_position,
                    anchor: new_position,
                    match_bracket: None,
                    grep_position: None,
                }
            } else {
                CursorState {
                    position: new_position,
                    anchor: start_pos,
                    match_bracket: None,
                    grep_position: None,
                }
            };
            let last_touched_line = start_pos.line + newlines;
            Some((
                start_char,
                end_char_excl,
                new_text,
                nc,
                a.min(start_pos.line),
                b.max(last_touched_line),
            ))
        }
    };

    let Some((start_char, end_char, new_text, new_cursor, edit_first, edit_last_incl)) = edit
    else {
        let revision = buf.revision;
        let response = wrap_for_response(&s, client_id, buffer_id, cursor);
        return Ok(EditResult {
            revision,
            cursor: response,
        });
    };

    let cursors_before: HashMap<ClientId, CursorState> = s
        .cursors
        .iter()
        .filter_map(|((c, bid), cs)| {
            if *bid == buffer_id {
                Some((*c, *cs))
            } else {
                None
            }
        })
        .collect();

    let was_dirty = s.buffers[&buffer_id].dirty;
    let revision = {
        let buf_mut = s.buffers.get_mut(&buffer_id).expect("just checked");
        buf_mut.apply_edit(
            start_char,
            end_char,
            &new_text,
            EditKindTag::Text,
            cursors_before,
        )
    };
    // Re-clamp the new cursor against the post-edit buffer (positions computed above used the
    // pre-edit buffer; if the edit shortened lines, clamp_position keeps them legal).
    let new_cursor = {
        let buf_mut = s.buffers.get_mut(&buffer_id).expect("just checked");
        let mut c = new_cursor;
        c.position = motion::clamp_position(buf_mut, c.position);
        c.anchor = motion::clamp_position(buf_mut, c.anchor);
        c
    };
    s.cursors.insert((client_id, buffer_id), new_cursor);
    s.clear_motion_history_for_buffer(buffer_id);
    s.clear_tree_selection_history_for_buffer(buffer_id);
    s.clear_virtual_col_for_buffer(buffer_id);

    let edit_last_excl = edit_last_incl + 1;
    let mut search_summary_pushes = promote_transient(&mut s, buffer_id);
    search_summary_pushes.extend(refresh_searches_for_buffer(&mut s, buffer_id));
    let new_line_count = s.buffers[&buffer_id].line_count();
    refresh_viewport_ranges_for_buffer(&mut s, buffer_id, new_line_count);
    let buf_ref = &s.buffers[&buffer_id];
    let mut pushes: PendingPushes = Vec::new();
    for vp in s.viewports.values() {
        if vp.buffer_id != buffer_id {
            continue;
        }
        if !vp.diff_view
            && !ranges_overlap(
                vp.first_logical_line,
                vp.last_logical_line_exclusive,
                edit_first,
                edit_last_excl,
            )
        {
            continue;
        }
        let Some(sender) = s.clients.get(&vp.client_id).map(|c| c.outbound.clone()) else {
            continue;
        };
        let search = s.searches.get(&(vp.client_id, buffer_id));
        pushes.push((
            sender,
            build_lines_changed_notif(
                buf_ref,
                vp,
                revision,
                search,
                buffer_both_hunks(&s, buffer_id),
                buffer_diagnostics(&s, buffer_id),
                buffer_git_status(&s, buffer_id),
            ),
        ));
    }

    let picker_pushes = maybe_refresh_dirty(&mut s, buffer_id, was_dirty);

    let new_cursor = wrap_for_response(&s, client_id, buffer_id, new_cursor);
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    for (sender, notif) in search_summary_pushes {
        let _ = sender.send(notif).await;
    }
    for (sender, notif) in picker_pushes {
        let _ = sender.send(notif).await;
    }
    Ok(EditResult {
        revision,
        cursor: new_cursor,
    })
}

/// Walk the cursor's ancestors looking for a tree-sitter node whose kind contains "comment"
/// and whose text starts with `open` and ends with `close`. Returns the node's byte range.
/// We match by kind-substring rather than exact name because grammars use different names
/// (`comment`, `block_comment`, `line_comment`, …) and the open/close suffix check validates
/// it's a block-style comment regardless.
fn find_enclosing_block_comment(
    tree: &tree_sitter::Tree,
    source: &[u8],
    byte: usize,
    open: &str,
    close: &str,
) -> Option<(usize, usize)> {
    let root = tree.root_node();
    let here = root.descendant_for_byte_range(byte, byte + 1)?;
    let mut node = Some(here);
    while let Some(n) = node {
        if n.kind().contains("comment") {
            let s = n.start_byte();
            let e = n.end_byte();
            let span = source.get(s..e)?;
            if span.starts_with(open.as_bytes())
                && span.ends_with(close.as_bytes())
                && e - s >= open.len() + close.len()
            {
                return Some((s, e));
            }
        }
        node = n.parent();
    }
    None
}

struct LineClassify {
    any_nonblank: bool,
    all_commented: bool,
    min_indent: usize,
}

fn classify_line_range(lines: &[String], prefix: Option<&str>) -> Option<LineClassify> {
    let prefix = prefix?;
    let mut all_commented = true;
    let mut min_indent: Option<usize> = None;
    let mut any_nonblank = false;
    for line in lines {
        let content = line.strip_suffix('\n').unwrap_or(line);
        let leading: usize = content
            .as_bytes()
            .iter()
            .take_while(|b| **b == b' ' || **b == b'\t')
            .count();
        let rest = &content[leading..];
        if rest.is_empty() {
            continue;
        }
        any_nonblank = true;
        min_indent = Some(min_indent.map_or(leading, |m| m.min(leading)));
        if !rest.starts_with(prefix) {
            all_commented = false;
        }
    }
    Some(LineClassify {
        any_nonblank,
        all_commented,
        min_indent: min_indent.unwrap_or(0),
    })
}

/// `true` when the selection doesn't cover a contiguous run of *complete* lines (lower
/// endpoint at col 0 of its line, upper endpoint at the line end of its line). Cursor-only
/// counts as non-partial. Partial selections — single mid-line, or multi-line where one of
/// the boundary lines isn't fully covered — route to block-comment when the language has it.
fn is_partial_line_selection(buf: &Buffer, cursor: &CursorState) -> bool {
    if cursor.is_point() {
        // A point cursor is a 1-char selection — single-line, definitionally non-partial
        // for the comment-toggle decision.
        return false;
    }
    let (lo, hi) = motion::ordered(cursor.position, cursor.anchor);
    let line_end_hi = motion::line_byte_len_excl_newline(buf, hi.line);
    let lo_at_start = lo.col == 0;
    // Accept either exclusive end (col == line_end) or inclusive last byte (col + 1 ==
    // line_end). Aether's selections are inclusive on both endpoints, so the natural
    // "whole-line" form has the cursor on the last byte.
    let hi_at_end = hi.col == line_end_hi || hi.col + 1 == line_end_hi;
    !(lo_at_start && hi_at_end)
}

/// `(start_pos, end_pos)` of the selection, both inclusive, ordered. For a point cursor both
/// endpoints land on the cursor's position.
fn ordered_selection_or_cursor_line(
    cursor: &CursorState,
) -> (
    aether_protocol::LogicalPosition,
    aether_protocol::LogicalPosition,
) {
    motion::ordered(cursor.position, cursor.anchor)
}

/// Endpoints `(line_start, line_end_inclusive)` for the content of `line_idx`, excluding the
/// trailing newline. Used to give "wrap the current line" a sensible char range when no
/// selection exists in a block-only language. Returns `None` for empty lines so the caller
/// can skip — otherwise a wrap on an empty line would replace its lone `\n` and merge the
/// line with the next.
fn current_line_content_endpoints(
    buf: &Buffer,
    line_idx: u32,
) -> Option<(
    aether_protocol::LogicalPosition,
    aether_protocol::LogicalPosition,
)> {
    let end_col = motion::line_byte_len_excl_newline(buf, line_idx);
    if end_col == 0 {
        return None;
    }
    Some((
        aether_protocol::LogicalPosition {
            line: line_idx,
            col: 0,
        },
        aether_protocol::LogicalPosition {
            line: line_idx,
            col: end_col - 1,
        },
    ))
}

fn line_edit_char_range(buf: &Buffer, a: u32, b: u32) -> (usize, usize) {
    let len_lines = buf.text.len_lines() as u32;
    let len_chars = buf.text.len_chars();
    let start_char = buf.text.line_to_char(a as usize);
    let end_char = if (b + 1) < len_lines {
        buf.text.line_to_char((b + 1) as usize)
    } else {
        len_chars
    };
    (start_char, end_char)
}

fn build_line_comment(
    lines: &[String],
    a: u32,
    prefix: &str,
    min_indent: usize,
) -> (String, HashMap<u32, i32>, HashMap<u32, usize>) {
    let prefix_with_space = format!("{prefix} ");
    let mut text = String::new();
    let mut shifts = HashMap::new();
    let mut insert_cols = HashMap::new();
    for (offset, line) in lines.iter().enumerate() {
        let line_idx = a + offset as u32;
        let (content, newline) = match line.strip_suffix('\n') {
            Some(s) => (s, "\n"),
            None => (line.as_str(), ""),
        };
        let leading: usize = content
            .as_bytes()
            .iter()
            .take_while(|b| **b == b' ' || **b == b'\t')
            .count();
        let is_blank = content[leading..].is_empty();
        if is_blank {
            text.push_str(content);
            text.push_str(newline);
            shifts.insert(line_idx, 0);
            insert_cols.insert(line_idx, leading);
            continue;
        }
        let (before, after) = content.split_at(min_indent);
        text.push_str(before);
        text.push_str(&prefix_with_space);
        text.push_str(after);
        text.push_str(newline);
        shifts.insert(line_idx, prefix_with_space.len() as i32);
        insert_cols.insert(line_idx, min_indent);
    }
    (text, shifts, insert_cols)
}

fn build_line_uncomment(
    lines: &[String],
    a: u32,
    prefix: &str,
) -> (String, HashMap<u32, i32>, HashMap<u32, usize>) {
    let mut text = String::new();
    let mut shifts = HashMap::new();
    let mut insert_cols = HashMap::new();
    for (offset, line) in lines.iter().enumerate() {
        let line_idx = a + offset as u32;
        let (content, newline) = match line.strip_suffix('\n') {
            Some(s) => (s, "\n"),
            None => (line.as_str(), ""),
        };
        let leading: usize = content
            .as_bytes()
            .iter()
            .take_while(|b| **b == b' ' || **b == b'\t')
            .count();
        let rest = &content[leading..];
        if rest.is_empty() {
            text.push_str(content);
            text.push_str(newline);
            shifts.insert(line_idx, 0);
            insert_cols.insert(line_idx, leading);
            continue;
        }
        // We've already classified the range as `all_commented` so this strip is safe.
        let after_prefix = rest.strip_prefix(prefix).unwrap_or(rest);
        let (stripped_tail, removed) = if let Some(after_space) = after_prefix.strip_prefix(' ') {
            (after_space, prefix.len() + 1)
        } else {
            (after_prefix, prefix.len())
        };
        text.push_str(&content[..leading]);
        text.push_str(stripped_tail);
        text.push_str(newline);
        shifts.insert(line_idx, -(removed as i32));
        insert_cols.insert(line_idx, leading);
    }
    (text, shifts, insert_cols)
}

fn shift_cursor_by_line_map(
    cursor: CursorState,
    a: u32,
    b: u32,
    shifts: &HashMap<u32, i32>,
    insert_cols: &HashMap<u32, usize>,
) -> CursorState {
    // When a selection exists, treat its endpoints asymmetrically so the selection *extends*
    // to cover any prefix we just added (rather than sliding with the content and leaving the
    // new prefix outside the selection). The lower endpoint stays put when it sits exactly at
    // the insert column; the upper endpoint shifts forward to follow the content.
    let lower = motion::ordered(cursor.position, cursor.anchor).0;

    let shift_pos = |p: aether_protocol::LogicalPosition, is_lower_endpoint: bool| {
        if p.line < a || p.line > b {
            return p;
        }
        let shift = shifts.get(&p.line).copied().unwrap_or(0);
        let insert_col = insert_cols.get(&p.line).copied().unwrap_or(0) as u32;
        if p.col < insert_col {
            return p;
        }
        let col = if shift >= 0 {
            // The endpoint that anchors the selection's *start* stays at insert_col so the
            // selection grows; everything else (including cursor-only) shifts forward.
            if is_lower_endpoint && p.col == insert_col {
                p.col
            } else {
                p.col.saturating_add(shift as u32)
            }
        } else {
            let removed = (-shift) as u32;
            let prefix_end = insert_col + removed;
            if p.col >= prefix_end {
                p.col - removed
            } else {
                insert_col
            }
        };
        aether_protocol::LogicalPosition { line: p.line, col }
    };

    // Don't clamp here; positions are post-edit, and the post-edit clamp at the call site
    // handles legality. Clamping against the pre-edit buffer would clip to shorter lines.
    let position_is_lower = lower == cursor.position;
    let position = shift_pos(cursor.position, position_is_lower);
    let anchor_is_lower = lower == cursor.anchor;
    let anchor = shift_pos(cursor.anchor, anchor_is_lower);
    CursorState {
        position,
        anchor,
        match_bracket: None,
        grep_position: None,
    }
}

pub async fn input_dedent(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: CountedEditParams,
) -> Result<EditResult, RpcError> {
    let mut last = None;
    for _ in 0..params.count.max(1) {
        last = Some(apply_indent_or_dedent(state, ctx, params.buffer_id, IndentKind::Dedent).await?);
    }
    Ok(last.expect("count.max(1) iterations"))
}

#[derive(Clone, Copy)]
enum IndentKind {
    Indent,
    Dedent,
}

/// Per-buffer-style soft indent. Selection's line range gets the prefix added (or stripped, on
/// dedent). Cursor and anchor are shifted by the per-line delta — on indent that's always
/// +unit.len(); on dedent it's 0/-1/-unit.len() depending on what was actually there to strip.
async fn apply_indent_or_dedent(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    buffer_id: BufferId,
    kind: IndentKind,
) -> Result<EditResult, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    let buf = s
        .buffers
        .get(&buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let indent = buf.indent_style.unit();
    let cursor = s
        .cursors
        .get(&(client_id, buffer_id))
        .copied()
        .unwrap_or_default();

    let (start, end) = motion::ordered(cursor.position, cursor.anchor);
    let (a, b) = (start.line, end.line);

    let len_lines = buf.text.len_lines() as u32;
    let len_chars = buf.text.len_chars();
    let start_char = buf.text.line_to_char(a as usize);
    let end_char = if (b + 1) < len_lines {
        buf.text.line_to_char((b + 1) as usize)
    } else {
        len_chars
    };

    // Build the replacement text and a per-line column shift map.
    let mut new_text = String::new();
    let mut shifts: HashMap<u32, i32> = HashMap::new();
    let mut any_changed = false;
    for line_idx in a..=b {
        let line_str: String = buf.text.line(line_idx as usize).chunks().collect();
        let (content, newline) = match line_str.strip_suffix('\n') {
            Some(s) => (s, "\n"),
            None => (line_str.as_str(), ""),
        };
        let (modified, shift): (String, i32) = match kind {
            IndentKind::Indent => (format!("{indent}{content}"), indent.len() as i32),
            IndentKind::Dedent => {
                if let Some(s) = content.strip_prefix(indent.as_ref()) {
                    (s.to_string(), -(indent.len() as i32))
                } else if let Some(s) = content.strip_prefix(' ') {
                    (s.to_string(), -1)
                } else {
                    (content.to_string(), 0)
                }
            }
        };
        if shift != 0 {
            any_changed = true;
        }
        shifts.insert(line_idx, shift);
        new_text.push_str(&modified);
        new_text.push_str(newline);
    }

    if !any_changed {
        return Ok(EditResult {
            revision: buf.revision,
            cursor,
        });
    }

    let cursors_before: HashMap<ClientId, CursorState> = s
        .cursors
        .iter()
        .filter_map(|((c, bid), cs)| {
            if *bid == buffer_id {
                Some((*c, *cs))
            } else {
                None
            }
        })
        .collect();

    let was_dirty = s.buffers[&buffer_id].dirty;
    let (revision, new_cursor) = {
        let buf_mut = s.buffers.get_mut(&buffer_id).expect("just checked");
        let revision = buf_mut.apply_edit(
            start_char,
            end_char,
            &new_text,
            EditKindTag::Text,
            cursors_before,
        );

        let shift_pos = |p: aether_protocol::LogicalPosition| {
            let shift = shifts.get(&p.line).copied().unwrap_or(0);
            let col = if shift >= 0 {
                p.col.saturating_add(shift as u32)
            } else {
                p.col.saturating_sub((-shift) as u32)
            };
            aether_protocol::LogicalPosition { line: p.line, col }
        };
        let new_cursor = CursorState {
            position: motion::clamp_position(buf_mut, shift_pos(cursor.position)),
            anchor: motion::clamp_position(buf_mut, shift_pos(cursor.anchor)),
            match_bracket: None,
            grep_position: None,
        };
        (revision, new_cursor)
    };
    s.cursors.insert((client_id, buffer_id), new_cursor);
    s.clear_motion_history_for_buffer(buffer_id);
    s.clear_tree_selection_history_for_buffer(buffer_id);
    s.clear_virtual_col_for_buffer(buffer_id);

    let edit_first = a;
    let edit_last_excl = b + 1;
    let mut search_summary_pushes = promote_transient(&mut s, buffer_id);
    search_summary_pushes.extend(refresh_searches_for_buffer(&mut s, buffer_id));
    let new_line_count = s.buffers[&buffer_id].line_count();
    refresh_viewport_ranges_for_buffer(&mut s, buffer_id, new_line_count);
    let buf_ref = &s.buffers[&buffer_id];
    let mut pushes: PendingPushes = Vec::new();
    for vp in s.viewports.values() {
        if vp.buffer_id != buffer_id {
            continue;
        }
        if !vp.diff_view
            && !ranges_overlap(
                vp.first_logical_line,
                vp.last_logical_line_exclusive,
                edit_first,
                edit_last_excl,
            )
        {
            continue;
        }
        let Some(sender) = s.clients.get(&vp.client_id).map(|c| c.outbound.clone()) else {
            continue;
        };
        let search = s.searches.get(&(vp.client_id, buffer_id));
        pushes.push((
            sender,
            build_lines_changed_notif(
                buf_ref,
                vp,
                revision,
                search,
                buffer_both_hunks(&s, buffer_id),
                buffer_diagnostics(&s, buffer_id),
                buffer_git_status(&s, buffer_id),
            ),
        ));
    }

    let picker_pushes = maybe_refresh_dirty(&mut s, buffer_id, was_dirty);

    let new_cursor = wrap_for_response(&s, client_id, buffer_id, new_cursor);
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    for (sender, notif) in search_summary_pushes {
        let _ = sender.send(notif).await;
    }
    for (sender, notif) in picker_pushes {
        let _ = sender.send(notif).await;
    }
    Ok(EditResult {
        revision,
        cursor: new_cursor,
    })
}

pub async fn input_move_lines(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: InputMoveLinesParams,
) -> Result<EditResult, RpcError> {
    // The repeat loop lives server-side.
    let mut last = None;
    for _ in 0..params.count.max(1) {
        last = Some(input_move_lines_once(state, ctx, &params).await?);
    }
    Ok(last.expect("count.max(1) iterations"))
}

async fn input_move_lines_once(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: &InputMoveLinesParams,
) -> Result<EditResult, RpcError> {
    let client_id = ctx.client_id;
    let buffer_id = params.buffer_id;

    // Phase 1: read state and compute the edit while holding the lock.
    let mut s = state.lock().await;
    let buf = s
        .buffers
        .get(&buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let cursor = s
        .cursors
        .get(&(client_id, buffer_id))
        .copied()
        .unwrap_or_default();

    // Selection's line range: the lines the user wants to move.
    let (start_pos, end_pos) = motion::ordered(cursor.position, cursor.anchor);
    let (a, b) = (start_pos.line, end_pos.line);

    // The "last real line" — ropey counts a trailing empty line after a final newline that's not
    // user-visible; treat it as out-of-bounds for move purposes.
    let line_count = buf.line_count();
    let len_bytes = buf.text.len_bytes();
    let trailing_newline = len_bytes > 0 && buf.text.byte(len_bytes - 1) == b'\n';
    let last_real_line = if len_bytes == 0 {
        0
    } else if trailing_newline {
        line_count.saturating_sub(2)
    } else {
        line_count.saturating_sub(1)
    };

    let can_move = match params.direction {
        VerticalDirection::Down => b < last_real_line,
        VerticalDirection::Up => a > 0,
    };
    if !can_move {
        return Ok(EditResult {
            revision: buf.revision,
            cursor,
        });
    }

    // Compute the swap. `slice_top` contains the lines that come first in the original layout,
    // `slice_bottom` the lines that come second; we emit them in reverse. The only subtlety is
    // when the trailing slice doesn't end in '\n' (i.e. it's the buffer's final line without a
    // trailing newline): we have to move that newline-or-its-absence to the new last slice.
    let len_lines = buf.text.len_lines() as u32;
    let len_chars = buf.text.len_chars();
    let (edit_start, edit_end, new_text, line_delta) = match params.direction {
        VerticalDirection::Down => {
            let a_start = buf.text.line_to_char(a as usize);
            let bp1_start = buf.text.line_to_char((b + 1) as usize);
            let bp2_start = if (b + 2) <= len_lines {
                buf.text.line_to_char((b + 2) as usize)
            } else {
                len_chars
            };
            let slice_top: String = buf.text.slice(a_start..bp1_start).to_string();
            let slice_bottom: String = buf.text.slice(bp1_start..bp2_start).to_string();
            let new_text = swap_segments(&slice_top, &slice_bottom);
            (a_start, bp2_start, new_text, 1i32)
        }
        VerticalDirection::Up => {
            let am1_start = buf.text.line_to_char((a - 1) as usize);
            let a_start = buf.text.line_to_char(a as usize);
            let bp1_start = if (b + 1) <= len_lines {
                buf.text.line_to_char((b + 1) as usize)
            } else {
                len_chars
            };
            let slice_top: String = buf.text.slice(am1_start..a_start).to_string();
            let slice_bottom: String = buf.text.slice(a_start..bp1_start).to_string();
            let new_text = swap_segments(&slice_top, &slice_bottom);
            (am1_start, bp1_start, new_text, -1i32)
        }
    };

    // Snapshot per-client cursors so undo can restore them.
    let cursors_before: HashMap<ClientId, CursorState> = s
        .cursors
        .iter()
        .filter_map(|((c, bid), cs)| {
            if *bid == buffer_id {
                Some((*c, *cs))
            } else {
                None
            }
        })
        .collect();

    let was_dirty = s.buffers[&buffer_id].dirty;
    let (revision, new_cursor) = {
        let buf_mut = s.buffers.get_mut(&buffer_id).expect("just checked");
        let revision = buf_mut.apply_edit(
            edit_start,
            edit_end,
            &new_text,
            EditKindTag::Text,
            cursors_before,
        );

        // Shift the requesting client's cursor (position + anchor) by `line_delta`. Other
        // clients' cursors are clamped by the standard post-edit clamp below.
        let shift = |p: aether_protocol::LogicalPosition| aether_protocol::LogicalPosition {
            line: (p.line as i32 + line_delta).max(0) as u32,
            col: p.col,
        };
        let new_cursor = CursorState {
            position: motion::clamp_position(buf_mut, shift(cursor.position)),
            anchor: motion::clamp_position(buf_mut, shift(cursor.anchor)),
            match_bracket: None,
            grep_position: None,
        };
        (revision, new_cursor)
    };
    s.cursors.insert((client_id, buffer_id), new_cursor);
    s.clear_motion_history_for_buffer(buffer_id);
    s.clear_tree_selection_history_for_buffer(buffer_id);
    s.clear_virtual_col_for_buffer(buffer_id);

    // Affected line range for viewport notifications.
    let (edit_first, edit_last_excl) = match params.direction {
        VerticalDirection::Down => (a, b + 2),
        VerticalDirection::Up => (a - 1, b + 1),
    };

    let mut search_summary_pushes = promote_transient(&mut s, buffer_id);
    search_summary_pushes.extend(refresh_searches_for_buffer(&mut s, buffer_id));
    let new_line_count = s.buffers[&buffer_id].line_count();
    refresh_viewport_ranges_for_buffer(&mut s, buffer_id, new_line_count);
    let buf_ref = &s.buffers[&buffer_id];
    let mut pushes: PendingPushes = Vec::new();
    for vp in s.viewports.values() {
        if vp.buffer_id != buffer_id {
            continue;
        }
        if !vp.diff_view
            && !ranges_overlap(
                vp.first_logical_line,
                vp.last_logical_line_exclusive,
                edit_first,
                edit_last_excl,
            )
        {
            continue;
        }
        let Some(sender) = s.clients.get(&vp.client_id).map(|c| c.outbound.clone()) else {
            continue;
        };
        let search = s.searches.get(&(vp.client_id, buffer_id));
        pushes.push((
            sender,
            build_lines_changed_notif(
                buf_ref,
                vp,
                revision,
                search,
                buffer_both_hunks(&s, buffer_id),
                buffer_diagnostics(&s, buffer_id),
                buffer_git_status(&s, buffer_id),
            ),
        ));
    }

    let picker_pushes = maybe_refresh_dirty(&mut s, buffer_id, was_dirty);

    let new_cursor = wrap_for_response(&s, client_id, buffer_id, new_cursor);
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    for (sender, notif) in search_summary_pushes {
        let _ = sender.send(notif).await;
    }
    for (sender, notif) in picker_pushes {
        let _ = sender.send(notif).await;
    }
    Ok(EditResult {
        revision,
        cursor: new_cursor,
    })
}

/// Build a new string with `bottom` first, then `top`, preserving "this is the last line of the
/// buffer and has no trailing newline" semantics. `top` is always followed by content so it ends
/// with '\n'; `bottom` ends with '\n' iff it's not the final segment of the buffer.
fn swap_segments(top: &str, bottom: &str) -> String {
    if bottom.ends_with('\n') {
        let mut s = String::with_capacity(top.len() + bottom.len());
        s.push_str(bottom);
        s.push_str(top);
        s
    } else {
        // `bottom` was the last line without a trailing '\n'. After the swap it sits in the
        // middle and needs a '\n' added; `top` takes the last-line spot and loses its '\n'.
        let mut s = String::with_capacity(top.len() + bottom.len() + 1);
        s.push_str(bottom);
        s.push('\n');
        s.push_str(top.strip_suffix('\n').unwrap_or(top));
        s
    }
}

pub async fn input_join_lines(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: CountedEditParams,
) -> Result<EditResult, RpcError> {
    // The repeat loop lives server-side (`3J` = one round-trip).
    let mut last = None;
    for _ in 0..params.count.max(1) {
        last = Some(input_join_lines_once(state, ctx, &params).await?);
    }
    Ok(last.expect("count.max(1) iterations"))
}

async fn input_join_lines_once(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: &CountedEditParams,
) -> Result<EditResult, RpcError> {
    let client_id = ctx.client_id;
    let buffer_id = params.buffer_id;

    // Figure out which line(s) we're joining. If the cursor has a selection that spans multiple
    // lines, join all of them. Otherwise, join the cursor's line with the one below.
    let (first_line, last_line) = {
        let s = state.lock().await;
        let cursor = s
            .cursors
            .get(&(client_id, buffer_id))
            .copied()
            .unwrap_or_default();
        let (a, b) = motion::ordered(cursor.position, cursor.anchor);
        let buf = s
            .buffers
            .get(&buffer_id)
            .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
        let line_count = buf.line_count();
        let first = a.line;
        // If single line, join with the line below it. If multi-line selection, join through
        // last selected line.
        let last = if a.line == b.line {
            a.line.saturating_add(1)
        } else {
            b.line
        };
        let last = last.min(line_count.saturating_sub(1));
        (first, last)
    };

    if first_line >= last_line {
        // Nothing to join (we're on the last line).
        let s = state.lock().await;
        let buf = &s.buffers[&buffer_id];
        return Ok(EditResult {
            revision: buf.revision,
            cursor: s
                .cursors
                .get(&(client_id, buffer_id))
                .copied()
                .unwrap_or_default(),
        });
    }

    // Compute the joined range, in char offsets. For each pair of consecutive lines, the range
    // to replace is `[end_of_trailing_ws_on_line_i, first_non_ws_on_line_i+1)` — replaced with
    // a single space. We do them in a single sweep on the rope.
    let s = state.lock().await;
    let buf = &s.buffers[&buffer_id];

    // Build the full replacement: walk the lines from `first_line` to `last_line`, concatenating
    // each line's content (stripped of trailing whitespace) plus a single space between.
    let mut joined = String::new();
    for line_idx in first_line..=last_line {
        let line_slice = buf.text.line(line_idx as usize);
        let mut text: String = line_slice.chunks().collect();
        if text.ends_with('\n') {
            text.pop();
        }
        if line_idx == first_line {
            // Keep first line's content, drop trailing whitespace.
            joined.push_str(text.trim_end());
        } else {
            joined.push(' ');
            // Drop leading whitespace on continuation lines; keep trailing untouched until
            // the next loop iteration trims it.
            let trimmed_start = text.trim_start();
            // For the last line, keep trailing whitespace as it normally appears.
            if line_idx == last_line {
                joined.push_str(trimmed_start);
            } else {
                joined.push_str(trimmed_start.trim_end());
            }
        }
    }

    // Determine the range to replace (full first..=last lines).
    let first_char = buf.text.line_to_char(first_line as usize);
    let last_line_end_char = if (last_line as usize + 1) < buf.text.len_lines() {
        // Up to (but not including) the \n at the end of `last_line`.
        let next_start = buf.text.line_to_char(last_line as usize + 1);
        next_start - 1
    } else {
        buf.text.len_chars()
    };
    drop(s);

    let cursors_before: HashMap<ClientId, CursorState> = {
        let s = state.lock().await;
        s.cursors
            .iter()
            .filter_map(|((c, b), cs)| {
                if *b == buffer_id {
                    Some((*c, *cs))
                } else {
                    None
                }
            })
            .collect()
    };

    let (revision, new_cursor, was_dirty) = {
        let mut s = state.lock().await;
        let was_dirty = s.buffers[&buffer_id].dirty;
        let buf = s.buffers.get_mut(&buffer_id).expect("just checked");
        let revision = buf.apply_edit(
            first_char,
            last_line_end_char,
            &joined,
            EditKindTag::Text,
            cursors_before,
        );
        let new_cursor_char = first_char + joined.chars().count();
        let new_pos = motion::char_to_pos(buf, new_cursor_char);
        let new_cursor = CursorState {
            position: new_pos,
            anchor: new_pos,
            match_bracket: None,
            grep_position: None,
        };
        s.cursors.insert((client_id, buffer_id), new_cursor);
        s.clear_motion_history_for_buffer(buffer_id);
        s.clear_tree_selection_history_for_buffer(buffer_id);
        s.clear_virtual_col_for_buffer(buffer_id);
        (revision, new_cursor, was_dirty)
    };

    // Push viewport/lines_changed for affected viewports (we changed multiple lines).
    let (pushes, search_summary_pushes, picker_pushes, new_cursor): (Vec<_>, Vec<_>, Vec<_>, _) = {
        let mut s = state.lock().await;
        let mut search_summary_pushes = promote_transient(&mut s, buffer_id);
        search_summary_pushes.extend(refresh_searches_for_buffer(&mut s, buffer_id));
        let new_line_count = s.buffers[&buffer_id].line_count();
        refresh_viewport_ranges_for_buffer(&mut s, buffer_id, new_line_count);
        let buf = &s.buffers[&buffer_id];
        let mut pushes = Vec::new();
        for vp in s.viewports.values() {
            if vp.buffer_id != buffer_id {
                continue;
            }
            let Some(sender) = s.clients.get(&vp.client_id).map(|c| c.outbound.clone()) else {
                continue;
            };
            let search = s.searches.get(&(vp.client_id, buffer_id));
            pushes.push((
                sender,
                build_lines_changed_notif(
                    buf,
                    vp,
                    revision,
                    search,
                    buffer_both_hunks(&s, buffer_id),
                    buffer_diagnostics(&s, buffer_id),
                    buffer_git_status(&s, buffer_id),
                ),
            ));
        }
        let picker_pushes = maybe_refresh_dirty(&mut s, buffer_id, was_dirty);
        let new_cursor = wrap_for_response(&s, client_id, buffer_id, new_cursor);
        (pushes, search_summary_pushes, picker_pushes, new_cursor)
    };

    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    for (sender, notif) in search_summary_pushes {
        let _ = sender.send(notif).await;
    }
    for (sender, notif) in picker_pushes {
        let _ = sender.send(notif).await;
    }

    Ok(EditResult {
        revision,
        cursor: new_cursor,
    })
}

#[derive(Clone, Copy)]
enum UndoDirection {
    Undo,
    Redo,
}

async fn apply_undo_or_redo(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    buffer_id: BufferId,
    direction: UndoDirection,
) -> Result<UndoResult, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;

    // Snapshot current cursors so the *other* direction's stack can restore them later.
    let current_cursors: HashMap<ClientId, CursorState> = s
        .cursors
        .iter()
        .filter_map(|((c, b), cs)| {
            if *b == buffer_id {
                Some((*c, *cs))
            } else {
                None
            }
        })
        .collect();

    let was_dirty = s.buffers.get(&buffer_id).map(|b| b.dirty).unwrap_or(false);
    let outcome = {
        let buf = s
            .buffers
            .get_mut(&buffer_id)
            .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
        match direction {
            UndoDirection::Undo => buf.undo(current_cursors),
            UndoDirection::Redo => buf.redo(current_cursors),
        }
    };

    let Some(outcome) = outcome else {
        // Nothing to undo/redo. Echo current cursor and revision back.
        let buf = s.buffers.get(&buffer_id).expect("just checked");
        let cursor = s
            .cursors
            .get(&(client_id, buffer_id))
            .copied()
            .unwrap_or_default();
        return Ok(UndoResult {
            revision: buf.revision,
            applied: false,
            cursor,
        });
    };

    let buf = s.buffers.get(&buffer_id).expect("just modified");
    let revision = buf.revision;

    // Restore cursors from the snapshot, clamped to valid positions in the restored rope.
    let mut new_cursors: HashMap<ClientId, CursorState> = HashMap::new();
    for (cid, cursor) in &outcome.restored_cursors {
        new_cursors.insert(*cid, clamp_cursor(buf, *cursor));
    }
    // Clients with cursors on this buffer that weren't in the snapshot: just clamp their current
    // cursor to the new buffer bounds.
    let existing_cursor_ids: Vec<ClientId> = s
        .cursors
        .keys()
        .filter_map(|(c, b)| if *b == buffer_id { Some(*c) } else { None })
        .collect();
    for cid in existing_cursor_ids {
        if let std::collections::hash_map::Entry::Vacant(e) = new_cursors.entry(cid) {
            if let Some(cursor) = s.cursors.get(&(cid, buffer_id)).copied() {
                e.insert(clamp_cursor(buf, cursor));
            }
        }
    }
    for (cid, cursor) in &new_cursors {
        s.cursors.insert((*cid, buffer_id), *cursor);
    }
    s.clear_motion_history_for_buffer(buffer_id);
    s.clear_tree_selection_history_for_buffer(buffer_id);
    s.clear_virtual_col_for_buffer(buffer_id);
    let undoing_cursor = new_cursors
        .get(&client_id)
        .copied()
        .unwrap_or_else(CursorState::default);

    // Push the full visible window to every viewport on this buffer — the rope was swapped
    // wholesale, so we can't be surgical about it.
    let mut search_summary_pushes = promote_transient(&mut s, buffer_id);
    search_summary_pushes.extend(refresh_searches_for_buffer(&mut s, buffer_id));
    let new_line_count = s.buffers[&buffer_id].line_count();
    refresh_viewport_ranges_for_buffer(&mut s, buffer_id, new_line_count);
    // LSP: the rope was swapped wholesale — tell the server so its diagnostics aren't stale.
    notify_lsp_change(&mut s, buffer_id);
    let buf_ref = &s.buffers[&buffer_id];
    let mut pushes: PendingPushes = Vec::new();
    for vp in s.viewports.values() {
        if vp.buffer_id != buffer_id {
            continue;
        }
        let Some(sender) = s.clients.get(&vp.client_id).map(|c| c.outbound.clone()) else {
            continue;
        };
        let search = s.searches.get(&(vp.client_id, buffer_id));
        pushes.push((
            sender,
            build_lines_changed_notif(
                buf_ref,
                vp,
                revision,
                search,
                buffer_both_hunks(&s, buffer_id),
                buffer_diagnostics(&s, buffer_id),
                buffer_git_status(&s, buffer_id),
            ),
        ));
    }

    let picker_pushes = maybe_refresh_dirty(&mut s, buffer_id, was_dirty);

    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    for (sender, notif) in search_summary_pushes {
        let _ = sender.send(notif).await;
    }
    for (sender, notif) in picker_pushes {
        let _ = sender.send(notif).await;
    }

    Ok(UndoResult {
        revision,
        applied: true,
        cursor: undoing_cursor,
    })
}

fn clamp_cursor(buf: &Buffer, cursor: CursorState) -> CursorState {
    let position = motion::clamp_position(buf, cursor.position);
    let anchor = motion::clamp_position(buf, cursor.anchor);
    CursorState {
        position,
        anchor,
        match_bracket: None,
        grep_position: None,
    }
}

/// Populate `match_bracket` on a cursor that's about to cross the wire. Looks up the bracket
/// pair (if any) at the cursor's position and stamps it onto the state. `match_bracket` is
/// never stored in `state.cursors`; it's purely a derived per-response field that drives the
/// client's match-bracket highlight overlay.
fn with_match_bracket(buf: &Buffer, mut cursor: CursorState) -> CursorState {
    let Some(syntax) = buf.syntax.as_ref() else {
        return cursor;
    };
    let byte = buf
        .text
        .char_to_byte(motion::pos_to_char(buf, cursor.position));
    if let Some((open, close)) = crate::brackets::find_match_bracket(&syntax.tree, byte) {
        let open_pos = motion::char_to_pos(buf, buf.text.byte_to_char(open));
        let close_pos = motion::char_to_pos(buf, buf.text.byte_to_char(close));
        cursor.match_bracket = Some((open_pos, close_pos));
    }
    cursor
}

/// Populate `grep_position` on a cursor that's about to cross the wire. The cursor counts as
/// "on" a hit when its selection covers *exactly* the match — `anchor` at the match's first
/// char and `position` at its last char (orientation-agnostic) — same strictness as
/// `match_index_for_cursor` uses to gate the in-buffer `A/B` counter. Any motion that grows,
/// shrinks, or shifts the selection drops the indicator on the next response.
fn with_grep_position(
    s: &ServerState,
    client_id: ClientId,
    buffer_id: BufferId,
    mut cursor: CursorState,
) -> CursorState {
    let Some(picker) = s.pickers.get(&(client_id, PickerKind::Grep)) else {
        return cursor;
    };
    let picker_state::PickerCandidates::Grep(ref hits) = picker.candidates else {
        return cursor;
    };
    if hits.is_empty() {
        return cursor;
    }
    let Some(buf) = s.buffers.get(&buffer_id) else {
        return cursor;
    };
    let Some(project) = s.active_project(client_id) else {
        return cursor;
    };
    let Some((current_idx, current_rel)) = buf.canonical_path.as_deref().and_then(|p| {
        crate::workspace_index::project_relative_parts(std::path::Path::new(p), &project.paths)
    }) else {
        return cursor;
    };
    // Compare in char-index space so multi-byte content stays on char boundaries (mirrors
    // `match_index_for_cursor`).
    let anchor_char = motion::pos_to_char(buf, cursor.anchor);
    let pos_char = motion::pos_to_char(buf, cursor.position);
    let sel_start_char = anchor_char.min(pos_char);
    let sel_end_char = anchor_char.max(pos_char);
    let total = hits.len() as u32;
    if let Some(idx) = hits.iter().position(|h| {
        if h.path_index != current_idx || h.relative_path != current_rel {
            return false;
        }
        let hit_start_pos = LogicalPosition {
            line: h.line,
            col: h.col,
        };
        let hit_end_excl_pos = LogicalPosition {
            line: h.line,
            col: h.col + h.match_byte_len,
        };
        let m_start_char = motion::pos_to_char(buf, hit_start_pos);
        let m_end_char_excl = motion::pos_to_char(buf, hit_end_excl_pos);
        let m_last_char = m_end_char_excl.saturating_sub(1).max(m_start_char);
        sel_start_char == m_start_char && sel_end_char == m_last_char
    }) {
        cursor.grep_position = Some(GrepPosition {
            current: (idx as u32).saturating_add(1),
            total,
        });
    }
    cursor
}

/// Same as `with_match_bracket` but starts from a `ServerState`: a one-liner for the many
/// handlers that need to populate the field just before returning. Safe if the buffer was
/// already dropped (returns the cursor unchanged). Also stamps `grep_position` if the client
/// has cached grep hits and the cursor is on one.
fn wrap_for_response(
    s: &ServerState,
    client_id: ClientId,
    buffer_id: BufferId,
    cursor: CursorState,
) -> CursorState {
    let with_brackets = s
        .buffers
        .get(&buffer_id)
        .map(|buf| with_match_bracket(buf, cursor))
        .unwrap_or(cursor);
    with_grep_position(s, client_id, buffer_id, with_brackets)
}

enum EditKind {
    /// Insert `text` at the cursor. For a point cursor (Insert-mode typing, paste-before)
    /// this is a pure insert at `position` — no chars are replaced. For a range (paste-
    /// replace, Ctrl-c after delete), the selection is replaced with `text`. When
    /// `select_pasted` is true and the inserted text is non-empty, the post-edit cursor
    /// selects the inserted text.
    ReplaceWith { text: String, select_pasted: bool },
    /// Delete the current inclusive selection. For a point cursor this deletes the 1 char at
    /// `position`. Used by Normal-mode `Ctrl-d` / `Delete` / `Ctrl-c`, and by Insert-mode
    /// `Delete` (forward).
    DeleteSelection,
    /// Delete the char immediately before `cursor.position` and leave the cursor there. Used
    /// by Insert-mode `Backspace` — there's no meaningful selection in Insert mode and "delete
    /// the previous char" is its own gesture.
    Backspace,
    /// Delete the cursor's whole line — content and trailing newline. Insert-mode `Ctrl-d`.
    DeleteLine,
    /// Blank the cursor's line — content only, newline preserved. Insert-mode `Ctrl-c`.
    ChangeLine,
    /// Replace the cursor's line (content + newline) with `text`. Insert-mode `Ctrl-r`.
    ReplaceLine { text: String },
    /// Wrap the surround target with `open`…`close` (`Ctrl-s <delim>`). Modeled as a single replace
    /// of the target range with `open + <target text> + close` so it's one undo step. `line` selects
    /// the target: false → the selection (post-edit cursor re-selects the wrapped text), true → the
    /// cursor line's content (post-edit cursor collapses to a point past the close).
    Surround { open: char, close: char, line: bool },
    /// Strip the pair of chars hugging the surround target (`Ctrl-Alt-s`), replacing the outer range
    /// with the inner text. `line` matches `Surround`. `input_unsurround` guarantees a valid pair
    /// exists before issuing this — the no-op case never reaches here.
    Unsurround { line: bool },
}

/// Where the cursor lands after an edit.
enum PostEdit {
    /// Collapse to a point just past the inserted text. The default for typing, deletes, and
    /// line-replace.
    PointAfter,
    /// Select the inserted text minus `lead` chars at the front and `trail` at the back. Paste uses
    /// `(0, 0)` to select everything; selection-surround uses `(1, 1)` to skip the delimiters.
    Select { lead: usize, trail: usize },
    /// Collapse to a point at this absolute char offset (clamped to the edited line's content).
    /// Line surround/unsurround use this to keep the caret on the same character it was on before
    /// the delimiters were inserted/removed around it.
    PointAt(usize),
}

async fn apply_edit(
    state: &SharedState,
    client_id: ClientId,
    buffer_id: BufferId,
    edit: EditKind,
) -> Result<EditResult, RpcError> {
    // Phase 1: hold the lock for the whole edit; gather notification senders before dropping it.
    let mut s = state.lock().await;

    let buf = s
        .buffers
        .get(&buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let cursor = s
        .cursors
        .get(&(client_id, buffer_id))
        .copied()
        .unwrap_or_default();

    // Compute the char range to replace and the affected line range. The range_is_inclusive
    // flag (selection mode) extends end_char by 1 to cover the cursor's char under the block.
    struct EditRange {
        start_char: usize,
        end_char: usize,
        first_line: u32,
        last_line: u32,
    }
    let range: EditRange = match &edit {
        EditKind::ReplaceWith { .. } => {
            if cursor.is_point() {
                // Pure insert at the point — no chars replaced.
                let c = motion::pos_to_char(buf, cursor.position);
                EditRange {
                    start_char: c,
                    end_char: c,
                    first_line: cursor.position.line,
                    last_line: cursor.position.line,
                }
            } else {
                let (lo, hi) = motion::ordered(cursor.position, cursor.anchor);
                let sc = motion::pos_to_char(buf, lo);
                let ec = motion::pos_to_char(buf, hi)
                    .saturating_add(1)
                    .min(buf.text.len_chars());
                EditRange {
                    start_char: sc,
                    end_char: ec,
                    first_line: lo.line,
                    last_line: hi.line,
                }
            }
        }
        EditKind::DeleteSelection => {
            let (lo, hi) = motion::ordered(cursor.position, cursor.anchor);
            let sc = motion::pos_to_char(buf, lo);
            let ec = motion::pos_to_char(buf, hi)
                .saturating_add(1)
                .min(buf.text.len_chars());
            EditRange {
                start_char: sc,
                end_char: ec,
                first_line: lo.line,
                last_line: hi.line,
            }
        }
        EditKind::Backspace => {
            let prev = motion::resolve_motion(
                buf,
                cursor.position,
                &Motion::Char {
                    direction: Direction::Backward,
                    count: 1,
                },
            );
            let (lo, hi) = motion::ordered(cursor.position, prev);
            let sc = motion::pos_to_char(buf, lo);
            let ec = motion::pos_to_char(buf, hi);
            EditRange {
                start_char: sc,
                end_char: ec,
                first_line: lo.line,
                last_line: hi.line,
            }
        }
        EditKind::DeleteLine | EditKind::ReplaceLine { .. } => {
            let line = cursor.position.line as usize;
            let total_lines = buf.text.len_lines();
            let sc = buf.text.line_to_char(line);
            let ec = if line + 1 < total_lines {
                buf.text.line_to_char(line + 1)
            } else {
                buf.text.len_chars()
            };
            EditRange {
                start_char: sc,
                end_char: ec,
                first_line: line as u32,
                last_line: line as u32,
            }
        }
        EditKind::ChangeLine => {
            let line = cursor.position.line as usize;
            let sc = buf.text.line_to_char(line);
            // Char count excluding the trailing newline, if any.
            let line_slice = buf.text.line(line);
            let len_chars = line_slice.len_chars();
            let has_trailing_nl = len_chars > 0 && line_slice.char(len_chars - 1) == '\n';
            let content_chars = if has_trailing_nl {
                len_chars - 1
            } else {
                len_chars
            };
            EditRange {
                start_char: sc,
                end_char: sc + content_chars,
                first_line: line as u32,
                last_line: line as u32,
            }
        }
        EditKind::Surround { line, .. } => {
            // The open/close chars are prepended/appended via insert_text below; the range here is
            // just the text being wrapped. Line target → the line's content; selection target →
            // the selection's char span [start, end).
            if *line {
                let l = cursor.position.line as usize;
                let (sc, ec) = line_content_char_range(buf, l);
                EditRange {
                    start_char: sc,
                    end_char: ec,
                    first_line: l as u32,
                    last_line: l as u32,
                }
            } else {
                let (sc, ec) = current_selection_char_range(buf, &cursor);
                let lo = motion::char_to_pos(buf, sc);
                let hi = motion::char_to_pos(buf, ec.saturating_sub(1).max(sc));
                EditRange {
                    start_char: sc,
                    end_char: ec,
                    first_line: lo.line,
                    last_line: hi.line,
                }
            }
        }
        EditKind::Unsurround { line } => {
            // The range covers the delimiters plus the text between them; insert_text below drops
            // the first and last chars. `input_unsurround` has verified a real pair sits at those
            // ends, so the arithmetic can't underflow/overflow the buffer. Line target → the line's
            // full content (delimiters are its first/last chars); selection target → the selection
            // grown one char at each end to swallow the hugging delimiters.
            if *line {
                let l = cursor.position.line as usize;
                let (sc, ec) = line_content_char_range(buf, l);
                EditRange {
                    start_char: sc,
                    end_char: ec,
                    first_line: l as u32,
                    last_line: l as u32,
                }
            } else {
                let (sc, ec) = current_selection_char_range(buf, &cursor);
                let outer_start = sc - 1;
                let outer_end = ec + 1;
                let lo = motion::char_to_pos(buf, outer_start);
                let hi = motion::char_to_pos(buf, outer_end.saturating_sub(1));
                EditRange {
                    start_char: outer_start,
                    end_char: outer_end,
                    first_line: lo.line,
                    last_line: hi.line,
                }
            }
        }
    };
    // `insert_text` is what gets written over `[start_char, end_char)`; `post_edit` decides where
    // the cursor lands (see `PostEdit`).
    let (insert_text, post_edit): (Cow<str>, PostEdit) = match &edit {
        EditKind::ReplaceWith {
            text,
            select_pasted,
        } => (
            Cow::Borrowed(text.as_str()),
            if *select_pasted {
                PostEdit::Select { lead: 0, trail: 0 }
            } else {
                PostEdit::PointAfter
            },
        ),
        EditKind::ReplaceLine { text } => (Cow::Borrowed(text.as_str()), PostEdit::PointAfter),
        EditKind::DeleteSelection
        | EditKind::Backspace
        | EditKind::DeleteLine
        | EditKind::ChangeLine => (Cow::Borrowed(""), PostEdit::PointAfter),
        EditKind::Surround { open, close, line } => {
            let inner: String = buf
                .text
                .slice(range.start_char..range.end_char)
                .chars()
                .collect();
            let mut wrapped =
                String::with_capacity(inner.len() + open.len_utf8() + close.len_utf8());
            wrapped.push(*open);
            wrapped.push_str(&inner);
            wrapped.push(*close);
            // Selection target re-selects the inner text (skip the 1-char delimiters). Line target
            // keeps the caret on the same char: the open delimiter is inserted before it, so shift
            // the pre-edit caret right by one.
            let post = if *line {
                PostEdit::PointAt(motion::pos_to_char(buf, cursor.position) + 1)
            } else {
                PostEdit::Select { lead: 1, trail: 1 }
            };
            (Cow::Owned(wrapped), post)
        }
        EditKind::Unsurround { line } => {
            // The inner text is everything between the stripped delimiters — the outer range minus
            // one char at each end — for both targets.
            let inner: String = buf
                .text
                .slice(range.start_char + 1..range.end_char - 1)
                .chars()
                .collect();
            // Selection target re-selects the inner text. Line target keeps the caret on the same
            // char: the open delimiter before it is removed, so shift the pre-edit caret left by one
            // (clamped to the line content start below).
            let post = if *line {
                PostEdit::PointAt(motion::pos_to_char(buf, cursor.position).saturating_sub(1))
            } else {
                PostEdit::Select { lead: 0, trail: 0 }
            };
            (Cow::Owned(inner), post)
        }
    };

    let start_char = range.start_char;
    let end_char = range.end_char;
    let old_first_line = range.first_line;
    let old_last_line = range.last_line;
    let kind_tag = match &edit {
        EditKind::ReplaceWith { .. } | EditKind::ReplaceLine { .. } => EditKindTag::Text,
        EditKind::DeleteSelection
        | EditKind::Backspace
        | EditKind::DeleteLine
        | EditKind::ChangeLine => EditKindTag::Delete,
        EditKind::Surround { .. } | EditKind::Unsurround { .. } => EditKindTag::Surround,
    };

    // Snapshot all per-client cursors on this buffer so the undo entry can restore them.
    let cursors_before: HashMap<ClientId, CursorState> = s
        .cursors
        .iter()
        .filter_map(|((c, b), cs)| {
            if *b == buffer_id {
                Some((*c, *cs))
            } else {
                None
            }
        })
        .collect();

    // Mutate the buffer (rope edit + incremental reparse + undo-group bookkeeping).
    let buf_mut = s.buffers.get_mut(&buffer_id).expect("just checked");
    let was_dirty = buf_mut.dirty;
    let revision = buf_mut.apply_edit(start_char, end_char, &insert_text, kind_tag, cursors_before);

    // Compute the cursor's new position.
    let inserted_char_count = insert_text.chars().count();
    // A `Select` span only holds if it leaves a non-empty range (lead + trail < inserted count);
    // otherwise it degrades to a point just past the insert.
    let selection = match post_edit {
        PostEdit::Select { lead, trail } if lead + trail < inserted_char_count => {
            Some((lead, trail))
        }
        _ => None,
    };
    let new_cursor_state = if let Some((lead, trail)) = selection {
        // Select the inserted span. Block cursor on its last char.
        let anchor_char = start_char + lead;
        let last_char = start_char + inserted_char_count - 1 - trail;
        let anchor_pos = motion::char_to_pos(buf_mut, anchor_char);
        let position_pos = motion::char_to_pos(buf_mut, last_char);
        CursorState {
            position: position_pos,
            anchor: anchor_pos,
            match_bracket: None,
            grep_position: None,
        }
    } else {
        // Point cursor. `PointAt` keeps the caret on the same char (clamped to the edited line's
        // content); everything else lands just past the inserted text.
        let point_char = match post_edit {
            PostEdit::PointAt(c) => c.clamp(start_char, buf_mut.text.len_chars()),
            _ => start_char + inserted_char_count,
        };
        let post_pos = motion::char_to_pos(buf_mut, point_char);
        CursorState {
            position: post_pos,
            anchor: post_pos,
            match_bracket: None,
            grep_position: None,
        }
    };
    s.cursors.insert((client_id, buffer_id), new_cursor_state);
    s.clear_motion_history_for_buffer(buffer_id);
    s.clear_tree_selection_history_for_buffer(buffer_id);
    s.clear_virtual_col_for_buffer(buffer_id);

    // Recompute every active search on this buffer so the embedded `search_matches` in the
    // line-render data we're about to send out reflects the post-edit text.
    let mut search_summary_pushes = promote_transient(&mut s, buffer_id);
    search_summary_pushes.extend(refresh_searches_for_buffer(&mut s, buffer_id));

    // Recompute every viewport's pushed range against the new line count, so a mutation that
    // *grew* the buffer (e.g. typing a newline) extends the window to cover the new lines.
    let new_line_count = s.buffers[&buffer_id].line_count();
    refresh_viewport_ranges_for_buffer(&mut s, buffer_id, new_line_count);

    // Collect notifications for all viewports whose pushed range intersects the edit.
    let edit_first = old_first_line;
    let edit_last_excl = old_last_line.saturating_add(1);
    let buf_ref = &s.buffers[&buffer_id];
    let mut pushes: PendingPushes = Vec::new();
    for vp in s.viewports.values() {
        if vp.buffer_id != buffer_id {
            continue;
        }
        if !vp.diff_view
            && !ranges_overlap(
                vp.first_logical_line,
                vp.last_logical_line_exclusive,
                edit_first,
                edit_last_excl,
            )
        {
            continue;
        }
        let Some(sender) = s.clients.get(&vp.client_id).map(|c| c.outbound.clone()) else {
            continue;
        };
        let search = s.searches.get(&(vp.client_id, buffer_id));
        let notif = build_lines_changed_notif(
            buf_ref,
            vp,
            revision,
            search,
            buffer_both_hunks(&s, buffer_id),
            buffer_diagnostics(&s, buffer_id),
            buffer_git_status(&s, buffer_id),
        );
        pushes.push((sender, notif));
    }

    // Re-push any open Buffers pickers only when the dirty flag flipped (typically the first
    // edit after a save). The picker row renders dirty + display only, so per-keystroke edits
    // mid-burst don't need pushes.
    let picker_pushes = maybe_refresh_dirty(&mut s, buffer_id, was_dirty);

    // LSP: full-document sync.
    notify_lsp_change(&mut s, buffer_id);

    let new_cursor_state = wrap_for_response(&s, client_id, buffer_id, new_cursor_state);
    drop(s);

    for (sender, notif) in pushes {
        // If the receiver's gone, the client's connection has dropped; not our problem.
        let _ = sender.send(notif).await;
    }
    for (sender, notif) in search_summary_pushes {
        let _ = sender.send(notif).await;
    }
    for (sender, notif) in picker_pushes {
        let _ = sender.send(notif).await;
    }

    Ok(EditResult {
        revision,
        cursor: new_cursor_state,
    })
}

fn ranges_overlap(a_start: u32, a_end_excl: u32, b_start: u32, b_end_excl: u32) -> bool {
    a_start < b_end_excl && b_start < a_end_excl
}

fn build_lines_changed_notif(
    buffer: &Buffer,
    vp: &Viewport,
    revision: Revision,
    search: Option<&SearchEntry>,
    hunks: &[crate::git::DiffHunk],
    diagnostics: &[crate::lsp::diagnostics::BufferDiagnostic],
    git_status: Option<GitBufferStatus>,
) -> Notification {
    let line_count = buffer.line_count();
    let new_first = vp.first_logical_line.min(line_count);
    let new_last_excl = vp
        .last_logical_line_exclusive
        .min(line_count)
        .max(new_first);
    let window = render_window(
        buffer,
        new_first,
        new_last_excl,
        vp.wrap_geometry(),
        vp.rows,
        WindowDecorations {
            search,
            diff_view: vp.diff_view,
            hunks,
            diagnostics,
            git_status,
        },
    );
    let params = ViewportLinesChangedParams {
        viewport_id: vp.id,
        revision,
        range: LogicalLineRange {
            start_logical_line: vp.first_logical_line,
            end_logical_line_exclusive: vp.last_logical_line_exclusive,
        },
        total_visual_rows: window.total_visual_rows,
        first_visual_row: window.first_visual_row,
        max_line_width: window.max_line_width,
        replacement_lines: window.lines,
        line_count,
        max_scroll_logical_line: window.max_scroll_logical_line,
        git_status: window.git_status,
    };
    Notification {
        jsonrpc: JsonRpc,
        method: ViewportLinesChanged::NAME.into(),
        params: serde_json::to_value(params).expect("infallible"),
    }
}

// ---- picker/* ----------------------------------------------------------------------------------

/// Build the buffer-picker candidate list for `client_id`: every buffer belonging to the
/// client's active project, MRU first, then any project buffers the client hasn't touched yet
/// (e.g. opened by another client of the same project) in buffer-id order. `(scratch N)`
/// placeholder display for buffers without a path. Returns an empty list if the client has no
/// active project (the picker shouldn't be reachable without one, but the lookup stays defensive).
fn build_buffer_candidates(
    s: &ServerState,
    client_id: ClientId,
) -> Vec<picker_state::BufferCandidate> {
    let Some(project) = s.active_project(client_id) else {
        return Vec::new();
    };
    let project_name = project.name.clone();
    let roots = project.paths.clone();
    let belongs =
        |id: &BufferId| s.buffer_projects.get(id).map(|s| s.as_str()) == Some(&project_name);

    let mut out: Vec<picker_state::BufferCandidate> = Vec::with_capacity(s.buffers.len());
    let mut seen: std::collections::HashSet<BufferId> = std::collections::HashSet::new();

    for &id in &project.mru_buffers {
        if !belongs(&id) {
            continue;
        }
        let Some(buf) = s.buffers.get(&id) else {
            continue;
        };
        out.push(buffer_candidate(buf, &roots));
        seen.insert(id);
    }
    // Append any project buffers not in the MRU yet so the picker still surfaces them. Stable
    // order (by id) so the tail is deterministic.
    let mut leftovers: Vec<BufferId> = s
        .buffers
        .keys()
        .copied()
        .filter(|id| belongs(id) && !seen.contains(id))
        .collect();
    leftovers.sort_unstable();
    for id in leftovers {
        out.push(buffer_candidate(&s.buffers[&id], &roots));
    }
    out
}

fn buffer_candidate(buf: &Buffer, roots: &[std::path::PathBuf]) -> picker_state::BufferCandidate {
    let display = match buf.canonical_path.as_deref() {
        Some(p) => crate::workspace_index::project_relative_display(p, roots)
            .unwrap_or_else(|| p.display().to_string()),
        None => format!(
            "(scratch {})",
            buf.scratch_number.map(u64::from).unwrap_or(buf.id)
        ),
    };
    // The (root index, relative path) the client needs for an opener URL — `None` for scratch
    // buffers and files outside every root (display still falls back to the absolute path above).
    let path = buf
        .canonical_path
        .as_deref()
        .and_then(|p| crate::workspace_index::project_relative_parts(p, roots));
    picker_state::BufferCandidate {
        buffer_id: buf.id,
        display,
        status: buffer_dirty_state(buf),
        path,
        transient: buf.transient,
    }
}

/// Map a buffer's save/disk flags to the picker's [`BufferDirtyState`], highest precedence first:
/// removed on disk → changed on disk → unsaved local edits → clean. Mirrors the editor status
/// bar's dot so the picker and the status line always agree.
fn buffer_dirty_state(buf: &Buffer) -> BufferDirtyState {
    if buf.externally_deleted {
        BufferDirtyState::ExternallyDeleted
    } else if buf.externally_modified {
        BufferDirtyState::ExternallyModified
    } else if buf.dirty {
        BufferDirtyState::Unsaved
    } else {
        BufferDirtyState::Clean
    }
}

/// Rebuild candidates for every subscribed `Buffers` picker, re-rank under the existing query,
/// and collect the resulting `picker/update` pushes. Caller sends them after dropping the lock.
/// Cheap when no picker is open: a HashMap scan over `pickers` and an early return.
pub(crate) fn refresh_buffer_pickers(s: &mut ServerState) -> PendingPushes {
    // Collect client_ids with a *subscribed* Buffers picker. Skip the rest — they may still
    // have persisted state from a prior session, but they're not waiting for pushes.
    let client_ids: Vec<ClientId> = s
        .pickers
        .iter()
        .filter_map(|((c, k), p)| {
            (*k == PickerKind::Buffers && p.subscribed.is_some()).then_some(*c)
        })
        .collect();
    let mut pushes = Vec::new();
    for client_id in client_ids {
        let new_candidates = build_buffer_candidates(s, client_id);
        let ServerState {
            pickers,
            matcher,
            clients,
            ..
        } = &mut *s;
        let Some(picker) = pickers.get_mut(&(client_id, PickerKind::Buffers)) else {
            continue;
        };
        picker.candidates = picker_state::PickerCandidates::Buffers(new_candidates);
        picker.rerank(matcher);
        if let Some(window) = picker.subscribed.as_mut() {
            let total = picker.ranked.len() as u32;
            if window.offset >= total {
                window.offset = total.saturating_sub(window.limit);
            }
        }
        let Some(update) = picker_state::build_update(picker, matcher) else {
            continue;
        };
        let Some(sender) = clients.get(&client_id).map(|c| c.outbound.clone()) else {
            continue;
        };
        pushes.push((sender, picker_update_notif(update)));
    }
    pushes
}

/// Rebuild and re-push every subscribed `Projects` picker. Called after a project is created,
/// renamed, or deleted (by any client) so an open chooser elsewhere reflects the new set live.
/// Mirrors [`refresh_buffer_pickers`]; the candidate list is a disk read, so callers must have
/// already written the config change before invoking this.
pub(crate) fn refresh_project_pickers(s: &mut ServerState) -> PendingPushes {
    let client_ids: Vec<ClientId> = s
        .pickers
        .iter()
        .filter_map(|((c, k), p)| {
            (*k == PickerKind::Projects && p.subscribed.is_some()).then_some(*c)
        })
        .collect();
    if client_ids.is_empty() {
        return Vec::new();
    }
    // One disk read of the projects directory, shared by every subscribed picker.
    let names = match crate::config::list_project_names() {
        Ok(n) => n,
        Err(_) => return Vec::new(), // can't enumerate — leave the pickers as they are
    };
    let mut pushes = Vec::new();
    for client_id in client_ids {
        let candidates = names
            .iter()
            .cloned()
            .map(|name| picker_state::ProjectCandidate { name })
            .collect();
        let ServerState {
            pickers,
            matcher,
            clients,
            ..
        } = &mut *s;
        let Some(picker) = pickers.get_mut(&(client_id, PickerKind::Projects)) else {
            continue;
        };
        picker.candidates = picker_state::PickerCandidates::Projects(candidates);
        picker.rerank(matcher);
        if let Some(window) = picker.subscribed.as_mut() {
            let total = picker.ranked.len() as u32;
            if window.offset >= total {
                window.offset = total.saturating_sub(window.limit);
            }
        }
        let Some(update) = picker_state::build_update(picker, matcher) else {
            continue;
        };
        let Some(sender) = clients.get(&client_id).map(|c| c.outbound.clone()) else {
            continue;
        };
        pushes.push((sender, picker_update_notif(update)));
    }
    pushes
}

/// Rebuild and re-push every subscribed `LspServers` picker. Called whenever a server's status
/// changes (from `crate::lsp::manager`) so the open dialog's health glyphs update live — e.g.
/// `◐ → ●` as a restart completes. Mirrors [`refresh_buffer_pickers`].
pub fn refresh_lsp_server_pickers(s: &mut ServerState) -> PendingPushes {
    let client_ids: Vec<ClientId> = s
        .pickers
        .iter()
        .filter_map(|((c, k), p)| {
            (*k == PickerKind::LspServers && p.subscribed.is_some()).then_some(*c)
        })
        .collect();
    let mut pushes = Vec::new();
    for client_id in client_ids {
        let Some(roots) = s.active_project(client_id).map(|p| p.paths.clone()) else {
            continue;
        };
        let new_candidates = build_lsp_server_candidates(s, &roots);
        let ServerState {
            pickers,
            matcher,
            clients,
            ..
        } = &mut *s;
        let Some(picker) = pickers.get_mut(&(client_id, PickerKind::LspServers)) else {
            continue;
        };
        picker.candidates = picker_state::PickerCandidates::LspServers(new_candidates);
        picker.rerank(matcher);
        if let Some(window) = picker.subscribed.as_mut() {
            let total = picker.ranked.len() as u32;
            if window.offset >= total {
                window.offset = total.saturating_sub(window.limit);
            }
        }
        let Some(update) = picker_state::build_update(picker, matcher) else {
            continue;
        };
        let Some(sender) = clients.get(&client_id).map(|c| c.outbound.clone()) else {
            continue;
        };
        pushes.push((sender, picker_update_notif(update)));
    }
    pushes
}

pub(crate) fn picker_update_notif(params: PickerUpdateParams) -> Notification {
    Notification {
        jsonrpc: JsonRpc,
        method: PickerUpdate::NAME.into(),
        params: serde_json::to_value(params).expect("infallible"),
    }
}

/// If `buffer_id`'s dirty flag changed across the just-completed mutation, collect picker
/// refresh pushes. Caller captures `was_dirty` before the mutation; this reads the post-
/// mutation value and decides. No-op (no allocation, no rerank) when dirty didn't change —
/// the typical hot path during a typing burst.
fn maybe_refresh_dirty(s: &mut ServerState, buffer_id: BufferId, was_dirty: bool) -> PendingPushes {
    let now_dirty = s.buffers.get(&buffer_id).map(|b| b.dirty).unwrap_or(false);
    if now_dirty == was_dirty {
        Vec::new()
    } else {
        refresh_buffer_pickers(s)
    }
}

/// Resolve and validate an Explorer *anchor* — the committed directory the query peeks relative
/// to. Canonicalizes, enforces the project boundary, requires a directory, and computes the
/// (in-project) parent for Alt-h ascent. Errors propagate to the client (a bad `directory_path`
/// is a real navigation error), unlike a bad *peek* path, which just lists nothing.
fn resolve_explorer_anchor(
    raw: &std::path::Path,
    project_paths: &[std::path::PathBuf],
) -> Result<picker_state::ExplorerAnchorInfo, RpcError> {
    let in_project =
        |p: &std::path::Path| project_paths.iter().any(|r| p == r.as_path() || p.starts_with(r));
    let canonical = std::fs::canonicalize(raw)
        .map_err(|e| RpcError::invalid_path(format!("canonicalizing {}: {e}", raw.display())))?;
    if !in_project(&canonical) {
        return Err(RpcError::invalid_path(format!(
            "{} is outside the project's access boundary",
            canonical.display()
        )));
    }
    if !std::fs::metadata(&canonical)
        .map_err(RpcError::file_io)?
        .is_dir()
    {
        return Err(RpcError::invalid_path(format!(
            "{} is not a directory",
            canonical.display()
        )));
    }
    let parent = canonical
        .parent()
        .and_then(|p| in_project(p).then(|| p.display().to_string()));
    Ok(picker_state::ExplorerAnchorInfo {
        path: canonical.display().to_string(),
        parent,
    })
}

/// Build the Explorer listing for `query`, relative to the committed `anchor`. The query's path
/// part (everything up to the last `/`) selects the directory to list — `anchor/<path_part>`, the
/// "peek"; the filter part (after the last `/`) is applied later by the prefix matcher. Returns
/// the listing plus `peek_missing`: true when the path part doesn't resolve to an in-project
/// directory (mid-typing a not-yet-created path — the "+ Create" case), in which case the listing
/// is empty and `path` still names the intended target so a file-watcher refresh can't bind it to
/// an unrelated real directory. The client reads `peek_missing` to decide whether `dir/` offers
/// "+ Create directory" (the listing shows the *contents*, so it can't tell on its own).
fn build_explorer_peek(
    anchor: &std::path::Path,
    query: &str,
    project_paths: &[std::path::PathBuf],
    filters: &aether_protocol::picker::PickerFilters,
) -> (picker_state::ExplorerCandidates, bool) {
    let (path_part, _filter) = picker_state::explorer_query_split(query);
    let target = if path_part.is_empty() {
        anchor.to_path_buf()
    } else {
        anchor.join(path_part)
    };
    match std::fs::canonicalize(&target)
        .ok()
        .and_then(|c| build_explorer_candidates_for_canonical(&c, project_paths, filters).ok())
    {
        Some(listing) => (listing, false),
        None => (empty_explorer_listing(&target), true),
    }
}

fn empty_explorer_listing(target: &std::path::Path) -> picker_state::ExplorerCandidates {
    picker_state::ExplorerCandidates {
        path: target.display().to_string(),
        parent: None,
        entries: Vec::new(),
    }
}

/// Build the Explorer's peek listing plus the committed anchor it's relative to. Honors the same
/// project-boundary rules as `directory_list`. Used by `picker_view` for `PickerKind::Explorer`.
/// The anchor is the requested path *or* the persisted anchor (when the client omitted the path
/// on a scroll/resume) *or* the first project root (first ever open); the listing peeks from it
/// using the persisted query (empty on `reset`, since it's being wiped).
async fn build_explorer_candidates(
    state: &SharedState,
    client_id: ClientId,
    requested: Option<&str>,
    reset: bool,
    filters: &aether_protocol::picker::PickerFilters,
) -> Result<
    (
        picker_state::ExplorerCandidates,
        picker_state::ExplorerAnchorInfo,
        bool,
    ),
    RpcError,
> {
    // One lock pass: project roots + the explorer's committed anchor + its current query (which
    // drives the peek). On `reset` the query is being wiped, so peek from the anchor itself.
    let (project_paths, existing_anchor, query) = {
        let s = state.lock().await;
        let picker = s.pickers.get(&(client_id, PickerKind::Explorer));
        let existing_anchor = picker.and_then(|p| p.explorer_anchor.clone());
        let query = if reset {
            String::new()
        } else {
            picker.map(|p| p.query.clone()).unwrap_or_default()
        };
        (
            s.active_project_or_err(client_id)?.paths.clone(),
            existing_anchor,
            query,
        )
    };
    let anchor_raw: std::path::PathBuf = if let Some(p) = requested {
        std::path::PathBuf::from(p)
    } else if let Some(a) = &existing_anchor {
        std::path::PathBuf::from(&a.path)
    } else {
        project_paths
            .first()
            .cloned()
            .ok_or_else(|| RpcError::invalid_path("no project paths configured"))?
    };
    let anchor = resolve_explorer_anchor(&anchor_raw, &project_paths)?;
    let (listing, peek_missing) = build_explorer_peek(
        std::path::Path::new(&anchor.path),
        &query,
        &project_paths,
        filters,
    );
    Ok((listing, anchor, peek_missing))
}

/// Build the Roots-mode candidate list — one row per project root, sorted by basename. The
/// matcher haystack is the basename alone (the disambiguator the client renders is purely
/// presentational).
async fn build_explorer_roots(
    state: &SharedState,
    client_id: ClientId,
) -> Result<Vec<picker_state::RootCandidate>, RpcError> {
    let s = state.lock().await;
    let project = s.active_project_or_err(client_id)?;
    let mut out: Vec<picker_state::RootCandidate> = project
        .paths
        .iter()
        .enumerate()
        .map(|(i, p)| picker_state::RootCandidate {
            path_index: i as u32,
            absolute_path: p.display().to_string(),
            basename: p
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string(),
        })
        .collect();
    out.sort_by(|a, b| a.basename.cmp(&b.basename));
    Ok(out)
}

/// Sync variant: build `ExplorerCandidates` for an already-canonicalized directory path. Used
/// by the async `build_explorer_candidates` (after it has resolved the requested path) and by
/// the file-watcher's explorer refresh path (which iterates over already-canonical paths).
pub(crate) fn build_explorer_candidates_for_canonical(
    canonical: &std::path::Path,
    project_paths: &[std::path::PathBuf],
    filters: &aether_protocol::picker::PickerFilters,
) -> Result<picker_state::ExplorerCandidates, RpcError> {
    let in_project = |p: &std::path::Path| -> bool {
        project_paths
            .iter()
            .any(|root| p == root.as_path() || p.starts_with(root))
    };
    if !in_project(canonical) {
        return Err(RpcError::invalid_path(format!(
            "{} is outside the project's access boundary",
            canonical.display()
        )));
    }
    let metadata = std::fs::metadata(canonical).map_err(RpcError::file_io)?;
    if !metadata.is_dir() {
        return Err(RpcError::invalid_path(format!(
            "{} is not a directory",
            canonical.display()
        )));
    }
    let parent = canonical.parent().and_then(|p| {
        if in_project(p) {
            Some(p.display().to_string())
        } else {
            None
        }
    });
    // One repo-wide status pass per listing, keyed by leaf name (directories carry their
    // descendants' aggregated status). Empty when the directory isn't in a Git repo.
    let git_status = crate::git::dir_statuses(canonical);
    let mut entries: Vec<picker_state::ExplorerEntry> = Vec::new();
    let read = std::fs::read_dir(canonical).map_err(RpcError::file_io)?;
    for ent in read {
        let ent = match ent {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name = match ent.file_name().into_string() {
            Ok(s) => s,
            Err(_) => continue,
        };
        let is_dir = ent.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let git_status = git_status.get(&name).copied();
        // Filter chips. The explorer shows hidden + ignored entries by default (colour-tagged),
        // so its chips *hide* rather than include. `changed` keeps any non-clean, non-ignored
        // status — for a directory that's the aggregated descendant status, so ancestors of a
        // change stay navigable.
        if filters.hide_hidden && name.starts_with('.') {
            continue;
        }
        if filters.hide_ignored && git_status == Some(aether_protocol::git::GitStatus::Ignored) {
            continue;
        }
        if filters.changed_only
            && !matches!(git_status, Some(s) if s != aether_protocol::git::GitStatus::Ignored)
        {
            continue;
        }
        entries.push(picker_state::ExplorerEntry {
            name,
            is_dir,
            git_status,
        });
    }
    // Directories first, then files, each alphabetical — same order the file browser used.
    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.cmp(&b.name),
    });
    Ok(picker_state::ExplorerCandidates {
        path: canonical.display().to_string(),
        parent,
        entries,
    })
}

/// One-shot directory listing for status-line prompts (save-as cycling). Same boundary rules and
/// sort order as the Explorer picker, but without any per-client state — just canonicalize, read,
/// return. The client filters/cycles locally.
pub async fn directory_list(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: DirectoryListParams,
) -> Result<DirectoryListResult, RpcError> {
    let project_paths = {
        let s = state.lock().await;
        s.active_project_or_err(ctx.client_id)?.paths.clone()
    };
    let raw = std::path::PathBuf::from(&params.path);
    let canonical = std::fs::canonicalize(&raw)
        .map_err(|e| RpcError::invalid_path(format!("canonicalizing {}: {e}", raw.display())))?;
    // Prompt listings (save-as cycling) are never filter-scoped — pass the no-op default.
    let candidates = build_explorer_candidates_for_canonical(
        &canonical,
        &project_paths,
        &aether_protocol::picker::PickerFilters::default(),
    )?;
    Ok(DirectoryListResult {
        path: candidates.path,
        parent: candidates.parent,
        entries: candidates
            .entries
            .into_iter()
            .map(|e| DirectoryEntry {
                name: e.name,
                is_dir: e.is_dir,
            })
            .collect(),
    })
}

/// Create a directory (and any missing intermediates), enforcing the project boundary first so
/// a `../escape/newdir` request can't produce dirs above the project root. Returns the
/// canonical absolute path of the created dir — clients use it to navigate into the new dir.
pub async fn directory_create(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: DirectoryCreateParams,
) -> Result<DirectoryCreateResult, RpcError> {
    let raw = std::path::PathBuf::from(&params.path);
    // Resolve against the deepest existing ancestor so a not-yet-existing target is still a
    // canonical-shaped path we can boundary-check before any I/O.
    let resolved = canonicalize_partial(&raw)
        .map_err(|e| RpcError::invalid_path(format!("canonicalizing {}: {e}", raw.display())))?;
    {
        let s = state.lock().await;
        if !s.active_project_or_err(ctx.client_id)?.contains(&resolved) {
            return Err(RpcError::invalid_path(format!(
                "{} is outside the project's access boundary",
                resolved.display()
            )));
        }
    }
    std::fs::create_dir_all(&resolved).map_err(RpcError::file_io)?;
    Ok(DirectoryCreateResult {
        path: resolved.display().to_string(),
    })
}

/// Walk every subscribed Explorer picker; if its current path matches one of `affected_dirs`,
/// re-list the directory, re-rank under the existing query, and emit a `picker/update` push.
/// Called by the file-watcher event handler. Does sync I/O under the `ServerState` lock —
/// `read_dir` on a single directory is fast enough for a single-user editor.
pub(crate) fn refresh_explorers_for_dirs(
    s: &mut ServerState,
    affected_dirs: &std::collections::HashSet<std::path::PathBuf>,
) -> PendingPushes {
    if affected_dirs.is_empty() {
        return Vec::new();
    }
    // Snapshot which (client, picker_path, filters) triples need refresh before we mutate —
    // the rebuilt listing must honour the picker's active filter chips.
    let to_refresh: Vec<(
        ClientId,
        std::path::PathBuf,
        aether_protocol::picker::PickerFilters,
    )> = s
        .pickers
        .iter()
        .filter_map(|((cid, kind), picker)| {
            if *kind != PickerKind::Explorer || picker.subscribed.is_none() {
                return None;
            }
            let path = match &picker.candidates {
                picker_state::PickerCandidates::Explorer(e) => std::path::PathBuf::from(&e.path),
                _ => return None,
            };
            if affected_dirs.contains(&path) {
                Some((*cid, path, picker.filters.clone()))
            } else {
                None
            }
        })
        .collect();
    if to_refresh.is_empty() {
        return Vec::new();
    }
    let mut pushes = Vec::new();
    for (client_id, path, filters) in to_refresh {
        // Each picker's project may differ — re-fetch per client. Skip silently if the client
        // somehow lost its active project between subscribe and refresh.
        let Some(project_paths) = s.active_project(client_id).map(|p| p.paths.clone()) else {
            continue;
        };
        let new_candidates =
            match build_explorer_candidates_for_canonical(&path, &project_paths, &filters) {
                Ok(c) => c,
                Err(_) => continue, // dir removed or no longer in project; skip silently
            };
        let ServerState {
            pickers,
            matcher,
            clients,
            ..
        } = &mut *s;
        let Some(picker) = pickers.get_mut(&(client_id, PickerKind::Explorer)) else {
            continue;
        };
        picker.candidates = picker_state::PickerCandidates::Explorer(new_candidates);
        picker.rerank(matcher);
        if let Some(window) = picker.subscribed.as_mut() {
            let total = picker.ranked.len() as u32;
            if window.offset >= total {
                window.offset = total.saturating_sub(window.limit);
            }
        }
        let Some(update) = picker_state::build_update(picker, matcher) else {
            continue;
        };
        let Some(sender) = clients.get(&client_id).map(|c| c.outbound.clone()) else {
            continue;
        };
        pushes.push((sender, picker_update_notif(update)));
    }
    pushes
}

/// Listed paths of every subscribed Explorer picker whose directory sits inside one of `workdirs`.
/// The watcher feeds these back into its `affected_dirs` set so explorer entry colours refresh
/// after a Git operation (commit / stage / checkout) that changed status without touching any
/// working-tree file — the file-modify path already covers ordinary edits via the parent dir.
pub(crate) fn explorer_dirs_in_workdirs(
    s: &ServerState,
    workdirs: &std::collections::HashSet<std::path::PathBuf>,
) -> Vec<std::path::PathBuf> {
    s.pickers
        .iter()
        .filter_map(|((_, kind), picker)| {
            if *kind != PickerKind::Explorer || picker.subscribed.is_none() {
                return None;
            }
            let path = match &picker.candidates {
                picker_state::PickerCandidates::Explorer(e) => std::path::PathBuf::from(&e.path),
                _ => return None,
            };
            workdirs
                .iter()
                .any(|wd| path.starts_with(wd))
                .then_some(path)
        })
        .collect()
}

/// Per-file Git status for the Files picker, aligned to `files` by index. Resolves each project
/// root's repo status once (one `statuses()` per root), then looks each file up by its
/// root-relative path — no per-file repo discovery. `None` at an index for a clean file, a file
/// whose root isn't in a repo, or any libgit2 error.
fn build_file_git_status(
    files: &[crate::workspace_index::CachedFile],
    roots: &[std::path::PathBuf],
) -> Vec<Option<aether_protocol::git::GitStatus>> {
    let per_root: Vec<Option<crate::git::RepoStatus>> = roots
        .iter()
        .map(|r| crate::git::repo_status_for_root(r))
        .collect();
    files
        .iter()
        .map(|f| {
            per_root
                .get(f.path_index as usize)
                .and_then(|rs| rs.as_ref())
                .and_then(|rs| rs.status_of(&f.relative_path))
        })
        .collect()
}

pub async fn picker_view(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: PickerViewParams,
) -> Result<PickerViewResult, RpcError> {
    let client_id = ctx.client_id;

    // Build candidates outside the mutation phase. Files needs an async workspace walk;
    // Buffers reads ServerState directly. Grep starts empty — the candidate set is generated
    // on demand by `picker/query`'s spawned search. Explorer re-lists the requested directory
    // (or the previously-listed one on resume) every call, like Buffers — directories change.
    // Per-kind active-project gating. Projects is the *only* kind that's allowed before
    // activation — it's how the user gets a project active in the first place. Files / Buffers /
    // Grep / Explorer require an active project; their candidate builders all hit project-scoped
    // data and would error or return nothing without one.
    if !matches!(params.kind, PickerKind::Projects) {
        let s = state.lock().await;
        s.active_project_or_err(client_id)?;
    }
    // Explorer carries its committed anchor (path the query peeks relative to) + whether the peek
    // resolved, out of the candidate-building phase so the hydration phase below can persist both.
    let mut explorer_anchor_to_set: Option<(picker_state::ExplorerAnchorInfo, bool)> = None;
    let candidates = match params.kind {
        PickerKind::Files => {
            // Walk the workspace outside the global lock — on first call it can take seconds.
            // The `Arc<WorkspaceIndex>` clone is cheap; the walk itself is memoized inside.
            let (workspace_index, roots) = {
                let s = state.lock().await;
                let p = s.active_project_or_err(client_id)?;
                (p.workspace_index.clone(), p.paths.clone())
            };
            let files = workspace_index.files().await;
            // One Git status pass per project root, aligned to the file snapshot by index, computed
            // off the lock (statuses() walks the worktree). Empty for roots that aren't in a repo.
            let git_status = std::sync::Arc::new(build_file_git_status(&files, &roots));
            picker_state::PickerCandidates::Files { files, git_status }
        }
        PickerKind::Buffers => {
            let s = state.lock().await;
            picker_state::PickerCandidates::Buffers(build_buffer_candidates(&s, client_id))
        }
        PickerKind::Grep => picker_state::PickerCandidates::Grep(Vec::new()),
        PickerKind::Explorer => {
            if params.explorer_roots {
                picker_state::PickerCandidates::ExplorerRoots(
                    build_explorer_roots(state, client_id).await?,
                )
            } else {
                // The listing is built *before* the picker state is (re)hydrated, but it must
                // honour the filters that will be in effect: the caller's replacement set if
                // sent, else the persisted set (which `reset` is about to wipe — treat that as
                // default).
                let filters = match params.filters.clone() {
                    Some(f) => f,
                    None if params.reset => Default::default(),
                    None => {
                        let s = state.lock().await;
                        s.pickers
                            .get(&(client_id, PickerKind::Explorer))
                            .map(|p| p.filters.clone())
                            .unwrap_or_default()
                    }
                };
                let (listing, anchor, peek_missing) = build_explorer_candidates(
                    state,
                    client_id,
                    params.directory_path.as_deref(),
                    params.reset,
                    &filters,
                )
                .await?;
                explorer_anchor_to_set = Some((anchor, peek_missing));
                picker_state::PickerCandidates::Explorer(listing)
            }
        }
        PickerKind::Projects => {
            // Configured-project enumeration is a synchronous read of one directory under
            // `$XDG_CONFIG_HOME/aether/projects/`. No active-project check; works pre-activation.
            let names = crate::config::list_project_names()
                .map_err(|e| RpcError::internal(format!("listing projects: {e}")))?;
            picker_state::PickerCandidates::Projects(
                names
                    .into_iter()
                    .map(|name| picker_state::ProjectCandidate { name })
                    .collect(),
            )
        }
        PickerKind::Diagnostics => match params.buffer_id {
            // Fresh open: build from the buffer's current diagnostics.
            Some(buffer_id) => {
                let s = state.lock().await;
                picker_state::PickerCandidates::Diagnostics(build_diagnostic_candidates(
                    &s, buffer_id,
                ))
            }
            // Resume / scroll re-view: an empty placeholder; `preserve_existing` keeps the snapshot.
            None => picker_state::PickerCandidates::Diagnostics(Vec::new()),
        },
        PickerKind::LspServers => {
            // Rebuilt every view from the active project's servers — the set is tiny and statuses
            // change, so there's no snapshot to preserve.
            let s = state.lock().await;
            let roots = s.active_project_or_err(client_id)?.paths.clone();
            picker_state::PickerCandidates::LspServers(build_lsp_server_candidates(&s, &roots))
        }
        // References always starts empty: the `textDocument/references` resolve is slow (an LSP
        // round-trip), so the picker opens immediately and a spawned task (below, after the lock
        // is set up) fills it. On a fresh open the empty set is installed here; on a resume/scroll
        // re-view `preserve_existing` keeps the prior snapshot.
        PickerKind::References => picker_state::PickerCandidates::References(Vec::new()),
        // DocumentSymbols also resolves asynchronously (a `textDocument/documentSymbol` round-trip),
        // so it opens empty and the spawned task (below) fills it; resume/scroll re-views preserve
        // the prior snapshot via `preserve_existing`.
        PickerKind::DocumentSymbols => picker_state::PickerCandidates::Symbols(Vec::new()),
    };

    let mut s = state.lock().await;
    let key = (client_id, params.kind);

    // Pre-resolve cursor info if we'll use it for Grep centering. Done before borrowing
    // `pickers` out of `s` so we don't have to juggle conflicting borrows after the split.
    let cursor_centering_info: Option<(LogicalPosition, Option<(u32, String)>)> =
        match (params.kind, params.center_on_cursor_grep_hit) {
            (PickerKind::Grep, Some(buffer_id)) => {
                let cursor = s
                    .cursors
                    .get(&(client_id, buffer_id))
                    .copied()
                    .unwrap_or_default();
                let leading_edge = if (cursor.anchor.line, cursor.anchor.col)
                    <= (cursor.position.line, cursor.position.col)
                {
                    cursor.anchor
                } else {
                    cursor.position
                };
                let current_key = s.buffers.get(&buffer_id).and_then(|b| {
                    let project = s.active_project(client_id)?;
                    b.canonical_path.as_deref().and_then(|p| {
                        crate::workspace_index::project_relative_parts(
                            std::path::Path::new(p),
                            &project.paths,
                        )
                    })
                });
                Some((leading_edge, current_key))
            }
            _ => None,
        };

    // (Re-)hydrate picker state. `reset` always wipes; otherwise we keep whatever was persisted
    // from a prior `view`/`query`/`hide` cycle. Split-borrow `pickers` and `matcher` from `s`
    // so we can hold mutable references to both at once.
    let ServerState {
        pickers, matcher, ..
    } = &mut *s;
    if params.reset {
        pickers.remove(&key);
    }
    match pickers.entry(key) {
        std::collections::hash_map::Entry::Vacant(e) => {
            e.insert(picker_state::PickerState::new(candidates));
        }
        std::collections::hash_map::Entry::Occupied(mut o) => {
            let p = o.get_mut();
            // Files: the workspace index returns the same `Arc` until a refresh — skip the
            // rerank in that case. Buffers: the candidate set is fresh each call, always re-bind.
            // Grep: the persisted candidates *are* the prior search results — keep them on resume
            // (the caller passed an empty placeholder). Discard them only on `reset`, which was
            // handled by the `pickers.remove(&key)` call above. Explorer: fresh listing every call
            // (directory contents may have changed), so always re-bind and rerank.
            let preserve_existing = match (&p.candidates, &candidates) {
                (
                    picker_state::PickerCandidates::Files { files: a, .. },
                    picker_state::PickerCandidates::Files { files: b, .. },
                ) => Arc::ptr_eq(a, b),
                (
                    picker_state::PickerCandidates::Grep(_),
                    picker_state::PickerCandidates::Grep(_),
                ) => true,
                // Diagnostics: keep the snapshot taken on open across scroll/resume re-views (the
                // re-view sends an empty placeholder), like Grep.
                (
                    picker_state::PickerCandidates::Diagnostics(_),
                    picker_state::PickerCandidates::Diagnostics(_),
                ) => true,
                // References: keep the one-shot LSP snapshot across scroll/resume re-views (the
                // re-view sends an empty placeholder), like Diagnostics and Grep.
                (
                    picker_state::PickerCandidates::References(_),
                    picker_state::PickerCandidates::References(_),
                ) => true,
                // DocumentSymbols: keep the one-shot LSP snapshot across scroll/resume re-views, like
                // References and Diagnostics.
                (
                    picker_state::PickerCandidates::Symbols(_),
                    picker_state::PickerCandidates::Symbols(_),
                ) => true,
                _ => false,
            };
            if !preserve_existing {
                p.candidates = candidates;
                p.rerank(matcher);
            }
        }
    }
    let picker = pickers.get_mut(&key).expect("populated above");

    // Commit the resolved Explorer anchor (navigation moved the directory) + the peek-missing flag.
    // Only set for actual directory listings — Roots mode leaves the prior anchor untouched so
    // re-entering a root resumes where it was.
    if let Some((anchor, peek_missing)) = explorer_anchor_to_set {
        picker.explorer_anchor = Some(anchor);
        picker.explorer_peek_missing = peek_missing;
    }

    // Replace persisted filters when the caller sent a set (`None` keeps what hide left). A
    // change re-ranks; for Grep it also drops the cached hits — they were produced under the
    // old filters and the client's follow-up `picker/query` will respawn the search.
    if let Some(filters) = params.filters {
        if filters != picker.filters {
            picker.filters = filters;
            if let picker_state::PickerCandidates::Grep(_) = picker.candidates {
                picker.candidates = picker_state::PickerCandidates::Grep(Vec::new());
                picker.last_completed_search = None;
            }
            picker.rerank(matcher);
        }
    }

    // References / DocumentSymbols: a fresh open (`buffer_id` present, vs `None` on scroll/resume
    // re-views) kicks off the async LSP resolve. Mint an epoch, mark the picker loading, and
    // remember what to spawn once the lock is released — the picker is pushed empty + `ticking`
    // now, and the spawned task fills it.
    let async_resolve: Option<(PickerKind, BufferId, u64)> = match (params.kind, params.buffer_id) {
        (PickerKind::References | PickerKind::DocumentSymbols, Some(buffer_id)) => {
            let epoch = next_async_load_epoch();
            picker.pending_async_load = Some(epoch);
            Some((params.kind, buffer_id, epoch))
        }
        _ => None,
    };

    // Cursor-derived centering for Grep: resolve the nearest cached hit at-or-after the
    // cursor's leading selection edge and use it as the effective center_on (overriding any
    // client-passed item). Lets `Space g` land on the user's spot in the result list even when
    // the cursor isn't sitting on a hit exactly. The resolution is echoed back via
    // `effective_center_on` so the client knows what to highlight.
    let cursor_resolved_item: Option<PickerItem> =
        match (cursor_centering_info.as_ref(), &picker.candidates) {
            (Some((leading_edge, current_key)), picker_state::PickerCandidates::Grep(hits))
                if !hits.is_empty() =>
            {
                find_nearest_grep_hit(
                    hits,
                    current_key.as_ref().map(|(i, r)| (*i, r.as_str())),
                    *leading_edge,
                )
                .map(|c| PickerItem::GrepHit {
                    path_index: c.path_index,
                    relative_path: c.relative_path.clone(),
                    line: c.line,
                    col: c.col,
                    preview: c.preview.clone(),
                    match_indices: c.match_indices.clone(),
                })
            }
            _ => None,
        };

    // Resolve the window. `center_on` wins over `offset` and picks a frame containing the item;
    // we centre it (roughly) so a small navigation away keeps it on screen. Falls through to
    // `offset` if the item isn't currently ranked. The cursor-resolved item, when present,
    // takes precedence over the client-passed `center_on`.
    let limit = params.limit.max(1);
    let mut effective_offset = params.offset;
    let effective_center_on = cursor_resolved_item.or_else(|| params.center_on.clone());
    if let Some(item) = effective_center_on.as_ref() {
        if let Some(rank) = picker.rank_of(item) {
            let half = limit / 2;
            effective_offset = rank.saturating_sub(half);
        }
    }
    let total = picker.ranked.len() as u32;
    if effective_offset >= total {
        effective_offset = total.saturating_sub(limit);
    }
    picker.subscribed = Some(picker_state::SubscribedWindow {
        offset: effective_offset,
        limit,
    });

    // Build the initial push so the client doesn't have to wait for an async update to arrive
    // before it can render. Caller will treat the response and the notification as redundant.
    let mut update = picker_state::build_update(picker, matcher);
    // References / DocumentSymbols open empty while they resolve — mark the push `ticking` so the
    // client shows the loading state instead of an empty result set.
    if async_resolve.is_some() {
        if let Some(ref mut u) = update {
            u.ticking = true;
        }
    }
    // Echo the committed *anchor*, not the (possibly peeked) listing — the client pins its
    // breadcrumb and "+ Create" base to it while a path-peek query is active. Roots mode (no
    // Explorer listing) echoes `None`, as before.
    let (directory_path, directory_parent) = match (&picker.candidates, &picker.explorer_anchor) {
        (picker_state::PickerCandidates::Explorer(_), Some(a)) => {
            (Some(a.path.clone()), a.parent.clone())
        }
        _ => (None, None),
    };
    let result = PickerViewResult {
        query: picker.query.clone(),
        generation: picker.generation,
        total_candidates: picker.total_candidates(),
        effective_offset,
        effective_center_on,
        directory_path,
        directory_parent,
        filters: picker.filters.clone(),
        // Carry the window on the response too — see `PickerViewResult::update`. The push below
        // stays for redundancy (and for the async grep walk's later updates).
        update: update.clone(),
    };
    let outbound = s.clients.get(&client_id).map(|c| c.outbound.clone());
    drop(s);

    if let (Some(sender), Some(params)) = (outbound, update) {
        let _ = sender.send(picker_update_notif(params)).await;
    }

    // Kick off the async resolve now that the empty + loading state is on the wire.
    if let Some((kind, buffer_id, epoch)) = async_resolve {
        match kind {
            PickerKind::References => {
                spawn_reference_resolve(state.clone(), client_id, buffer_id, epoch)
            }
            PickerKind::DocumentSymbols => {
                spawn_symbol_resolve(state.clone(), client_id, buffer_id, epoch)
            }
            _ => {}
        }
    }

    Ok(result)
}

pub async fn picker_query(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: PickerQueryParams,
) -> Result<(), RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    let key = (client_id, params.kind);
    // Explorer re-lists the query-derived peek directory before reranking; grab the project roots
    // up front (an immutable borrow of `s`, before the split below hands out `pickers`/`matcher`).
    let explorer_project_paths = if matches!(params.kind, PickerKind::Explorer) {
        s.active_project(client_id).map(|p| p.paths.clone())
    } else {
        None
    };
    let ServerState {
        pickers, matcher, ..
    } = &mut *s;
    let Some(picker) = pickers.get_mut(&key) else {
        // No-op if the client never opened the picker. Could also error; silently dropping
        // matches the lenient style of other handlers.
        return Ok(());
    };
    // Grep cache: if the (query, filters) pair matches the search whose walk last completed,
    // the existing candidates are still valid. Bump generation (so any in-flight worker from a
    // prior query bails on its next batch) but skip the wipe + respawn. The initial push built
    // below will carry the cached items.
    let grep_cache_hit = matches!(params.kind, PickerKind::Grep)
        && picker
            .last_completed_search
            .as_ref()
            .is_some_and(|(q, f)| *q == params.query && *f == params.filters);
    picker.query = params.query;
    picker.filters = params.filters;
    picker.generation = params.generation;
    match params.kind {
        // Grep: the query *is* the search. On a cache miss, drop any prior results and let the
        // spawned worker (kicked off below, outside the lock) repopulate. On a cache hit, leave
        // candidates intact. Either way, the generation bump above invalidates any in-flight
        // worker from a previous query.
        PickerKind::Grep => {
            if !grep_cache_hit {
                picker.candidates = picker_state::PickerCandidates::Grep(Vec::new());
                picker.ranked.clear();
                picker.last_completed_search = None;
            }
        }
        // Explorer: the query is a path. Re-list the directory it peeks into (anchor + the path
        // part) before reranking, so typing `src/` descends and `src/ma` filters `src`. Skip in
        // Roots mode (candidates aren't a directory listing) and before the first view (no
        // anchor) — both just rerank the existing candidates.
        PickerKind::Explorer => {
            if let (
                picker_state::PickerCandidates::Explorer(_),
                Some(anchor),
                Some(project_paths),
            ) = (
                &picker.candidates,
                picker.explorer_anchor.clone(),
                explorer_project_paths.as_ref(),
            ) {
                let (listing, peek_missing) = build_explorer_peek(
                    std::path::Path::new(&anchor.path),
                    &picker.query,
                    project_paths,
                    &picker.filters,
                );
                picker.candidates = picker_state::PickerCandidates::Explorer(listing);
                picker.explorer_peek_missing = peek_missing;
            }
            picker.rerank(matcher);
        }
        _ => picker.rerank(matcher),
    }

    // After a query change, the prior `offset` may now be past the end of the result set. Clamp.
    if let Some(window) = picker.subscribed.as_mut() {
        let total = picker.ranked.len() as u32;
        if window.offset >= total {
            window.offset = total.saturating_sub(window.limit);
        }
    }

    // References / DocumentSymbols whose async resolve is still outstanding: a filter typed mid-load
    // reranks the (still empty) candidates, so without this the push would report "finished, 0
    // matches" and the picker would flash "No results" until the resolve lands. Keep it ticking.
    let async_loading = picker.pending_async_load.is_some();
    let mut update = picker_state::build_update(picker, matcher);
    let query_for_grep = picker.query.clone();
    let filters_for_grep = picker.filters.clone();
    let generation = picker.generation;
    let will_spawn_grep_search = matches!(params.kind, PickerKind::Grep)
        && query_for_grep.len() >= grep::MIN_QUERY_LEN
        && !grep_cache_hit;
    // Mark the initial push as ticking when we're about to spawn the search. Without this the
    // client would briefly see "0 hits, search finished" between sending the query and the
    // coordinator's first batch landing.
    if will_spawn_grep_search
        || (matches!(
            params.kind,
            PickerKind::References | PickerKind::DocumentSymbols
        ) && async_loading)
    {
        if let Some(ref mut u) = update {
            u.ticking = true;
        }
    }
    let outbound = s.clients.get(&client_id).map(|c| c.outbound.clone());
    let workspace_index_for_grep = if matches!(params.kind, PickerKind::Grep) {
        // Active-project lookup can fail in the (defensively-handled) case where the client
        // somehow lost its active project between opening the picker and querying it. Skip the
        // grep spawn in that case — there's nothing meaningful to search.
        s.active_project(client_id)
            .map(|p| (p.workspace_index.clone(), p.paths.clone()))
    } else {
        None
    };
    drop(s);

    if let (Some(sender), Some(params)) = (outbound, update) {
        let _ = sender.send(picker_update_notif(params)).await;
    }

    if will_spawn_grep_search {
        if let Some((workspace_index, roots)) = workspace_index_for_grep {
            let files = workspace_index.files().await;
            grep::spawn_search(
                state.clone(),
                files,
                roots,
                client_id,
                query_for_grep,
                filters_for_grep,
                generation,
            );
        }
    }
    Ok(())
}

pub async fn picker_select(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: PickerSelectParams,
) -> Result<PickerSelectResult, RpcError> {
    let client_id = ctx.client_id;
    let s = state.lock().await;
    let key = (client_id, params.kind);
    let picker = s.pickers.get(&key).ok_or_else(|| {
        RpcError::new(
            ErrorCode::INVALID_REQUEST,
            "no active picker for this client",
        )
    })?;
    picker_state::resolve_select(picker, &params.item).ok_or_else(|| {
        RpcError::invalid_params(
            "selected item is not in the picker's candidate set, or is not selectable",
        )
    })
}

pub async fn picker_hide(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: PickerHideParams,
) -> Result<(), RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    if let Some(picker) = s.pickers.get_mut(&(client_id, params.kind)) {
        picker.subscribed = None;
    }
    Ok(())
}

/// Step through the client's cached grep hits without re-opening the picker. See the protocol
/// doc on `PickerGrepNavigate` for the directional + virtual-insert rules. Returns `None` when
/// there are no cached hits or the cursor is past the last (or before the first) hit with no
/// further file in the requested direction — the client treats `None` as a no-op.
pub async fn picker_grep_navigate(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: PickerGrepNavigateParams,
) -> Result<Option<PickerGrepNavigateTarget>, RpcError> {
    let client_id = ctx.client_id;
    let s = state.lock().await;
    let buffer = s
        .buffers
        .get(&params.buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
    // None for scratch buffers; otherwise the project-relative display path the grep candidates
    // are keyed by. Falls back to None if the client somehow lost its active project — the
    // navigate handler then treats this as "no anchor in the file list" the same way scratch
    // buffers do.
    let current_key: Option<(u32, String)> = s.active_project(client_id).and_then(|project| {
        buffer.canonical_path.as_deref().and_then(|p| {
            crate::workspace_index::project_relative_parts(std::path::Path::new(p), &project.paths)
        })
    });

    let Some(picker) = s.pickers.get(&(client_id, PickerKind::Grep)) else {
        return Ok(None);
    };
    let picker_state::PickerCandidates::Grep(ref hits) = picker.candidates else {
        return Ok(None);
    };
    if hits.is_empty() {
        return Ok(None);
    }

    // Use the outer edge of the cursor's selection so a hit the cursor currently sits on is
    // treated as "current" and skipped. Without this, `<` from a freshly-jumped grep result
    // (where the selection covers the whole match) would land back on the same hit because
    // the hit's stored start position is < the cursor's end position.
    let cursor = s
        .cursors
        .get(&(client_id, params.buffer_id))
        .copied()
        .unwrap_or_default();
    let (min_edge, max_edge) =
        if (cursor.anchor.line, cursor.anchor.col) < (cursor.position.line, cursor.position.col) {
            (cursor.anchor, cursor.position)
        } else {
            (cursor.position, cursor.anchor)
        };
    let current_key_ref = current_key.as_ref().map(|(i, r)| (*i, r.as_str()));
    let target = match params.direction {
        Direction::Forward => find_next_grep_hit(hits, current_key_ref, max_edge),
        Direction::Backward => find_prev_grep_hit(hits, current_key_ref, min_edge),
    };
    let query = picker.query.clone();
    let options = picker.filters.match_options();
    let target = target.map(|c| {
        (
            c.path_index,
            c.relative_path.clone(),
            c.abs_path.clone(),
            LogicalPosition {
                line: c.line,
                col: c.col,
            },
        )
    });
    drop(s);
    let Some((path_index, relative_path, path, position)) = target else {
        return Ok(None);
    };
    // Composite post-step (docs/protocol-composites.md, J): open the hit — transient, at
    // the hit position, jump origin recorded, search primed — in the same round-trip.
    let opened = if params.open {
        Some(
            buffer_open(
                state,
                ctx,
                BufferOpenParams {
                    path_index: Some(path_index),
                    relative_path: Some(relative_path),
                    jump_to: Some(position),
                    transient: Some(true),
                    record_nav_from: Some(params.buffer_id),
                    prime_search: Some(query.clone()),
                    prime_search_options: options,
                    ..Default::default()
                },
            )
            .await?,
        )
    } else {
        None
    };
    Ok(Some(PickerGrepNavigateTarget {
        path,
        position,
        query,
        options,
        opened,
    }))
}

/// Index of the file-boundary hit to jump to within `hits` (grep candidates, grouped into
/// contiguous per-file runs). `from` is the selection's current index.
///
/// - Forward → the first hit whose `(path_index, relative_path)` differs from `from`'s, scanning
///   forward (i.e. the first hit of the next file).
/// - Backward → the start of `from`'s own file run; or, if `from` is already that start, the start
///   of the previous file's run (vim-`{`).
///
/// Returns `None` at the ends (no next file going forward; already on the very first hit going
/// backward). Pure + index-only so it's straightforward to unit-test.
fn grep_file_boundary(
    hits: &[picker_state::GrepHitCandidate],
    from: usize,
    direction: Direction,
) -> Option<usize> {
    let key = |i: usize| (hits[i].path_index, hits[i].relative_path.as_str());
    let cur = key(from);
    match direction {
        Direction::Forward => (from + 1..hits.len()).find(|&j| key(j) != cur),
        Direction::Backward => {
            // Walk back to the first hit of the current file.
            let mut run_start = from;
            while run_start > 0 && key(run_start - 1) == cur {
                run_start -= 1;
            }
            if from != run_start {
                return Some(run_start); // not at the top of this file → go there
            }
            if run_start == 0 {
                return None; // already on the very first hit
            }
            // At the top of this file → walk to the top of the previous file.
            let prev = key(run_start - 1);
            let mut p = run_start - 1;
            while p > 0 && key(p - 1) == prev {
                p -= 1;
            }
            Some(p)
        }
    }
}

/// Move the grep picker's selection to the first hit of the next / previous file. Computed against
/// the full cached hit list so it works past the client's over-fetch window; the client frames the
/// returned hit via `picker/view { center_on }`. `None` when there's no further file that way.
pub async fn picker_grep_file_jump(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: PickerGrepFileJumpParams,
) -> Result<Option<PickerItem>, RpcError> {
    let client_id = ctx.client_id;
    let s = state.lock().await;
    let Some(picker) = s.pickers.get(&(client_id, PickerKind::Grep)) else {
        return Ok(None);
    };
    let picker_state::PickerCandidates::Grep(ref hits) = picker.candidates else {
        return Ok(None);
    };
    if hits.is_empty() {
        return Ok(None);
    }
    let from = (params.from_index as usize).min(hits.len() - 1);
    let Some(target) = grep_file_boundary(hits, from, params.direction) else {
        return Ok(None);
    };
    let h = &hits[target];
    Ok(Some(PickerItem::GrepHit {
        path_index: h.path_index,
        relative_path: h.relative_path.clone(),
        line: h.line,
        col: h.col,
        preview: h.preview.clone(),
        match_indices: h.match_indices.clone(),
    }))
}

/// First grep hit "after" the cursor. Within the same file: the first hit whose `(line, col)` is
/// past the cursor's. Across files: the first hit whose `(path_index, relative_path)` sorts after
/// `current`. If the current buffer has no path (scratch or outside every root), every hit counts
/// as "after" and we return the first.
///
/// Assumes `hits` are roughly in `(path_index, relative_path, line, col)` order — true in
/// practice because the walker sorts files that way and ripgrep emits matches per file in line
/// order.
fn find_next_grep_hit<'a>(
    hits: &'a [picker_state::GrepHitCandidate],
    current: Option<(u32, &str)>,
    cursor: LogicalPosition,
) -> Option<&'a picker_state::GrepHitCandidate> {
    use std::cmp::Ordering;
    let Some((cur_idx, cur_rel)) = current else {
        return hits.first();
    };
    hits.iter().find(
        |h| match (h.path_index, h.relative_path.as_str()).cmp(&(cur_idx, cur_rel)) {
            Ordering::Greater => true,
            Ordering::Equal => (h.line, h.col) > (cursor.line, cursor.col),
            Ordering::Less => false,
        },
    )
}

fn find_prev_grep_hit<'a>(
    hits: &'a [picker_state::GrepHitCandidate],
    current: Option<(u32, &str)>,
    cursor: LogicalPosition,
) -> Option<&'a picker_state::GrepHitCandidate> {
    use std::cmp::Ordering;
    let Some((cur_idx, cur_rel)) = current else {
        return hits.last();
    };
    hits.iter().rev().find(|h| {
        match (h.path_index, h.relative_path.as_str()).cmp(&(cur_idx, cur_rel)) {
            Ordering::Less => true,
            Ordering::Equal => (h.line, h.col) < (cursor.line, cursor.col),
            Ordering::Greater => false,
        }
    })
}

/// First grep hit "at or after" the cursor in walker order, wrapping to the first hit overall
/// when nothing matches. Used by `picker/view`'s `center_on_cursor_grep_hit` to land the picker
/// on "where you are" in the result list even when the cursor isn't sitting on a match
/// exactly. Inclusive (a hit at exactly the cursor's position is the answer), unlike
/// `find_next_grep_hit` which is strict (`>` skips past the current).
fn find_nearest_grep_hit<'a>(
    hits: &'a [picker_state::GrepHitCandidate],
    current: Option<(u32, &str)>,
    cursor: LogicalPosition,
) -> Option<&'a picker_state::GrepHitCandidate> {
    use std::cmp::Ordering;
    let Some((cur_idx, cur_rel)) = current else {
        return hits.first();
    };
    hits.iter()
        .find(
            |h| match (h.path_index, h.relative_path.as_str()).cmp(&(cur_idx, cur_rel)) {
                Ordering::Greater => true,
                Ordering::Equal => (h.line, h.col) >= (cursor.line, cursor.col),
                Ordering::Less => false,
            },
        )
        .or_else(|| hits.first())
}

#[cfg(test)]
mod project_name_tests {
    use super::validate_project_name;

    #[test]
    fn trims_surrounding_whitespace_and_accepts() {
        assert_eq!(validate_project_name("  my-proj  ").unwrap(), "my-proj");
        assert_eq!(validate_project_name("aether").unwrap(), "aether");
    }

    #[test]
    fn rejects_empty_blank_and_path_separators() {
        for bad in ["", "   ", "a/b", "a\\b", ".", ".."] {
            assert!(
                validate_project_name(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }
}

#[cfg(test)]
mod diff_anchor_tests {
    use super::*;
    use crate::git::{ChangeKind, DiffHunk};

    fn hunk(kind: ChangeKind, anchor_line: u32, new_lines: u32, deleted: &[&str]) -> DiffHunk {
        DiffHunk {
            kind,
            anchor_line,
            new_lines,
            deleted: deleted.iter().map(|s| s.to_string()).collect(),
            old_start: 0, // irrelevant for render-anchoring tests
            stage: aether_protocol::viewport::DiffStage::Unstaged,
        }
    }

    #[test]
    fn modified_and_deleted_hunks_anchor_their_removed_text() {
        let hunks = vec![
            hunk(ChangeKind::Modified, 1, 1, &["old beta"]),
            hunk(ChangeKind::Deleted, 4, 0, &["gone one", "gone two"]),
            hunk(ChangeKind::Added, 7, 2, &[]), // additions contribute no phantom rows
        ];
        let map = deleted_rows_by_anchor(&hunks, 100);
        assert_eq!(map.get(&1).map(Vec::len), Some(1));
        assert_eq!(map[&1][0].text, "old beta");
        assert_eq!(map[&1][0].kind, VirtualRowKind::Deleted);
        assert_eq!(map.get(&4).map(Vec::len), Some(2));
        assert_eq!(map[&4][1].text, "gone two");
        assert!(!map.contains_key(&7), "pure additions have no deleted rows");
    }

    #[test]
    fn eof_deletion_clamps_to_last_line() {
        // A deletion anchored past the last line (e.g. removed the file's tail) clamps onto the
        // final line index so it still renders (above the trailing empty line of the buffer).
        let hunks = vec![hunk(ChangeKind::Deleted, 9, 0, &["tail"])];
        let map = deleted_rows_by_anchor(&hunks, 5); // line_count = 5 → last index 4
        assert!(!map.contains_key(&9));
        assert_eq!(map.get(&4).map(Vec::len), Some(1));
        assert_eq!(map[&4][0].text, "tail");
    }

    #[test]
    fn markers_cover_new_side_lines_and_deletion_anchors() {
        let hunks = vec![
            hunk(ChangeKind::Modified, 2, 1, &["was"]), // line 2 → Modified
            hunk(ChangeKind::Added, 5, 3, &[]),         // lines 5,6,7 → Added
            hunk(ChangeKind::Deleted, 9, 0, &["x"]),    // line 9 → Deleted (gutter flag)
        ];
        let map = diff_markers_by_line(&hunks, 100);
        let unstaged = aether_protocol::viewport::DiffStage::Unstaged;
        assert_eq!(map.get(&2), Some(&(DiffMarker::Modified, unstaged)));
        assert_eq!(map.get(&5), Some(&(DiffMarker::Added, unstaged)));
        assert_eq!(map.get(&7), Some(&(DiffMarker::Added, unstaged)));
        assert_eq!(map.get(&8), None);
        assert_eq!(map.get(&9), Some(&(DiffMarker::Deleted, unstaged)));
    }

    #[test]
    fn added_modified_marker_wins_over_a_deletion_anchor_on_the_same_line() {
        // A deletion anchored on a line that's also added/modified keeps the stronger marker.
        let hunks = vec![
            hunk(ChangeKind::Deleted, 3, 0, &["gone"]),
            hunk(ChangeKind::Modified, 3, 1, &["was"]),
        ];
        let map = diff_markers_by_line(&hunks, 100);
        let unstaged = aether_protocol::viewport::DiffStage::Unstaged;
        assert_eq!(map.get(&3), Some(&(DiffMarker::Modified, unstaged)));
    }

    #[test]
    fn overlapping_staged_and_unstaged_hunks_read_as_unstaged() {
        use aether_protocol::viewport::DiffStage;
        // Composed-view collision: line 3 staged-modified then modified again. The unstaged top
        // layer wins outright — the line reads as plain unstaged (no third state), regardless of
        // which order the hunks arrive in. Surrounding staged-only lines stay Staged.
        let mut staged = hunk(ChangeKind::Modified, 2, 3, &["a", "b", "c"]); // lines 2,3,4
        staged.stage = DiffStage::Staged;
        let unstaged = hunk(ChangeKind::Modified, 3, 1, &["b'"]); // line 3 only
        let map = diff_markers_by_line(&[staged.clone(), unstaged.clone()], 100);
        assert_eq!(
            map.get(&2),
            Some(&(DiffMarker::Modified, DiffStage::Staged))
        );
        assert_eq!(
            map.get(&3),
            Some(&(DiffMarker::Modified, DiffStage::Unstaged))
        );
        assert_eq!(
            map.get(&4),
            Some(&(DiffMarker::Modified, DiffStage::Staged))
        );
        // Order-independent: a staged hunk processed after the unstaged one changes nothing.
        let reversed = diff_markers_by_line(&[unstaged, staged], 100);
        assert_eq!(
            reversed.get(&3),
            Some(&(DiffMarker::Modified, DiffStage::Unstaged))
        );
    }

    #[test]
    fn deleted_rows_keep_only_the_unstaged_layer_at_a_shared_anchor() {
        use aether_protocol::viewport::DiffStage;
        // A staged and an unstaged deletion anchored at the same line: only the index's (unstaged)
        // text is shown — it's what a revert would restore. A staged deletion elsewhere keeps its
        // rows (with the staged tag).
        let mut staged = hunk(ChangeKind::Deleted, 1, 0, &["head text"]);
        staged.stage = DiffStage::Staged;
        let unstaged = hunk(ChangeKind::Deleted, 1, 0, &["index text"]);
        let mut staged_elsewhere = hunk(ChangeKind::Deleted, 5, 0, &["solo head text"]);
        staged_elsewhere.stage = DiffStage::Staged;
        let map = deleted_rows_by_anchor(&[staged, unstaged, staged_elsewhere], 10);
        let rows = &map[&1];
        assert_eq!(
            rows.len(),
            1,
            "staged layer suppressed at the shared anchor"
        );
        assert_eq!(
            (rows[0].text.as_str(), rows[0].stage),
            ("index text", DiffStage::Unstaged)
        );
        let solo = &map[&5];
        assert_eq!(
            (solo[0].text.as_str(), solo[0].stage),
            ("solo head text", DiffStage::Staged)
        );
    }

    #[test]
    fn change_counts_tally_lines_by_class() {
        // Added/Modified count new-side lines (`new_lines`); Deleted counts removed lines
        // (`deleted.len()`). A Modified hunk's replaced old lines ride its `modified` count and are
        // *not* also tallied as deletions.
        let hunks = vec![
            hunk(ChangeKind::Added, 5, 3, &[]),                // +3
            hunk(ChangeKind::Modified, 2, 1, &["a", "b"]),     // ~1 (2 old lines → 1 new)
            hunk(ChangeKind::Modified, 10, 2, &["c"]),         // ~2
            hunk(ChangeKind::Deleted, 9, 0, &["x", "y", "z"]), // -3
        ];
        let c = git_change_counts(&hunks);
        assert_eq!((c.added, c.modified, c.deleted), (3, 3, 3));
        assert!(git_change_counts(&[]).is_empty());
    }
}

#[cfg(test)]
mod subscribe_snapshot_tests {
    use super::*;
    use crate::lsp::diagnostics::BufferDiagnostic;
    use aether_protocol::viewport::{DiagnosticSeverity, ScrollPosition, WrapMode};
    use aether_protocol::LogicalPosition;
    use tokio::sync::Mutex;

    fn diag(line: u32, severity: DiagnosticSeverity) -> BufferDiagnostic {
        BufferDiagnostic {
            start: LogicalPosition { line, col: 0 },
            end: LogicalPosition { line, col: 1 },
            severity,
            message: "m".into(),
        }
    }

    /// State with one file-text buffer carrying `diags` and the given external-change flags. No
    /// language server is attached (so `lsp_status` snapshots as `None`). `viewport_subscribe` reads
    /// only buffer/diagnostic/lsp state, so no client session registration is needed.
    fn setup(
        diags: Vec<BufferDiagnostic>,
        externally_modified: bool,
        externally_deleted: bool,
    ) -> (SharedState, ClientId, BufferId) {
        let mut st = ServerState::new();
        let buffer_id = st.allocate_buffer_id();
        let mut buf = Buffer::scratch(buffer_id, None, 1);
        buf.text = ropey::Rope::from_str("alpha\nbeta\n");
        buf.externally_modified = externally_modified;
        buf.externally_deleted = externally_deleted;
        st.buffers.insert(buffer_id, buf);
        if !diags.is_empty() {
            st.diagnostics.insert(buffer_id, diags);
        }
        (Arc::new(Mutex::new(st)), uuid::Uuid::new_v4(), buffer_id)
    }

    fn sub_params(buffer_id: BufferId) -> ViewportSubscribeParams {
        ViewportSubscribeParams {
            buffer_id,
            cols: 80,
            rows: 24,
            overscan_rows: 0,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::None,
            continuation_marker_width: 0,
            tab_width: 4,
        }
    }

    #[tokio::test]
    async fn subscribe_snapshots_existing_diagnostic_counts() {
        // The regression: diagnostics computed before this viewport subscribed must still reach the
        // status bar. They now ride the subscribe response, not only the change-notification.
        let (state, client_id, buffer_id) = setup(
            vec![
                diag(0, DiagnosticSeverity::Error),
                diag(1, DiagnosticSeverity::Warning),
                diag(1, DiagnosticSeverity::Warning),
            ],
            false,
            false,
        );
        let mut ctx = ConnectionCtx { client_id };
        let res = viewport_subscribe(&state, &mut ctx, sub_params(buffer_id))
            .await
            .unwrap();
        let c = res.buffer_status.diagnostics;
        assert_eq!((c.errors, c.warnings), (1, 2));
    }

    #[tokio::test]
    async fn subscribe_snapshots_external_change_flags() {
        // A client that starts showing a buffer the watcher already flagged externally-modified must
        // see the flag immediately, not only on the next disk event.
        let (state, client_id, buffer_id) = setup(Vec::new(), true, false);
        let mut ctx = ConnectionCtx { client_id };
        let res = viewport_subscribe(&state, &mut ctx, sub_params(buffer_id))
            .await
            .unwrap();
        assert!(res.buffer_status.externally_modified);
        assert!(!res.buffer_status.externally_deleted);
    }

    #[tokio::test]
    async fn subscribe_to_clean_unbacked_buffer_snapshots_empty_status() {
        let (state, client_id, buffer_id) = setup(Vec::new(), false, false);
        let mut ctx = ConnectionCtx { client_id };
        let res = viewport_subscribe(&state, &mut ctx, sub_params(buffer_id))
            .await
            .unwrap();
        let s = &res.buffer_status;
        assert!(s.diagnostics.is_empty());
        assert!(!s.externally_modified && !s.externally_deleted);
        assert!(s.lsp_status.is_none());
    }
}

#[cfg(test)]
mod grep_boundary_tests {
    use super::*;

    fn hit(rel: &str, line: u32) -> picker_state::GrepHitCandidate {
        picker_state::GrepHitCandidate {
            path_index: 0,
            relative_path: rel.to_string(),
            abs_path: format!("/ws/{rel}"),
            line,
            col: 0,
            match_byte_len: 1,
            preview: String::new(),
            match_indices: Vec::new(),
        }
    }

    // Three files in walker order: a.rs (3 hits), b.rs (1 hit), c.rs (2 hits).
    fn sample() -> Vec<picker_state::GrepHitCandidate> {
        vec![
            hit("a.rs", 1),
            hit("a.rs", 5),
            hit("a.rs", 9), // indices 0,1,2
            hit("b.rs", 2), // index 3
            hit("c.rs", 1),
            hit("c.rs", 4), // indices 4,5
        ]
    }

    #[test]
    fn forward_jumps_to_next_files_first_hit() {
        let h = sample();
        // From anywhere within a.rs → the first hit of b.rs (index 3).
        assert_eq!(grep_file_boundary(&h, 0, Direction::Forward), Some(3));
        assert_eq!(grep_file_boundary(&h, 2, Direction::Forward), Some(3));
        // From b.rs → the first hit of c.rs (index 4).
        assert_eq!(grep_file_boundary(&h, 3, Direction::Forward), Some(4));
        // Within the last file → nothing further forward.
        assert_eq!(grep_file_boundary(&h, 4, Direction::Forward), None);
        assert_eq!(grep_file_boundary(&h, 5, Direction::Forward), None);
    }

    #[test]
    fn backward_goes_to_current_file_top_then_previous_file() {
        let h = sample();
        // Mid-file (a.rs, index 2) → top of a.rs (index 0).
        assert_eq!(grep_file_boundary(&h, 2, Direction::Backward), Some(0));
        assert_eq!(grep_file_boundary(&h, 1, Direction::Backward), Some(0));
        // Already on the top of c.rs (index 4) → top of the previous file b.rs (index 3).
        assert_eq!(grep_file_boundary(&h, 4, Direction::Backward), Some(3));
        // Top of b.rs (index 3) → top of a.rs (index 0).
        assert_eq!(grep_file_boundary(&h, 3, Direction::Backward), Some(0));
        // The very first hit → nothing further back.
        assert_eq!(grep_file_boundary(&h, 0, Direction::Backward), None);
    }
}

#[cfg(test)]
mod diagnostic_span_tests {
    use super::{diagnostic_counts, diagnostic_spans_on_line, navigate_diagnostic_target};
    use crate::lsp::diagnostics::BufferDiagnostic;
    use aether_protocol::lsp::DiagnosticDirection;
    use aether_protocol::viewport::DiagnosticSeverity;
    use aether_protocol::LogicalPosition;

    #[test]
    fn diagnostic_counts_tally_by_severity() {
        let mk = |severity| BufferDiagnostic {
            start: LogicalPosition { line: 0, col: 0 },
            end: LogicalPosition { line: 0, col: 1 },
            severity,
            message: "m".into(),
        };
        let diags = vec![
            mk(DiagnosticSeverity::Error),
            mk(DiagnosticSeverity::Error),
            mk(DiagnosticSeverity::Warning),
            mk(DiagnosticSeverity::Hint),
        ];
        let c = diagnostic_counts(&diags);
        assert_eq!((c.errors, c.warnings, c.infos, c.hints), (2, 1, 0, 1));
        assert!(diagnostic_counts(&[]).is_empty());
    }

    fn diag(l0: u32, c0: u32, l1: u32, c1: u32) -> BufferDiagnostic {
        BufferDiagnostic {
            start: LogicalPosition { line: l0, col: c0 },
            end: LogicalPosition { line: l1, col: c1 },
            severity: DiagnosticSeverity::Error,
            message: "m".into(),
        }
    }

    #[test]
    fn single_line_span_is_clipped_to_its_range() {
        let diags = [diag(2, 3, 2, 7)];
        assert!(diagnostic_spans_on_line(&diags, 1, 80).is_empty());
        let on = diagnostic_spans_on_line(&diags, 2, 80);
        assert_eq!(on.len(), 1);
        assert_eq!((on[0].start, on[0].end), (3, 7));
        assert!(diagnostic_spans_on_line(&diags, 3, 80).is_empty());
    }

    #[test]
    fn multi_line_span_covers_each_line() {
        // Lines 1..=3; line lengths 10/20/30.
        let diags = [diag(1, 4, 3, 6)];
        let start = diagnostic_spans_on_line(&diags, 1, 10);
        assert_eq!((start[0].start, start[0].end), (4, 10)); // from col to EOL
        let mid = diagnostic_spans_on_line(&diags, 2, 20);
        assert_eq!((mid[0].start, mid[0].end), (0, 20)); // whole line
        let end = diagnostic_spans_on_line(&diags, 3, 30);
        assert_eq!((end[0].start, end[0].end), (0, 6)); // up to col
    }

    #[test]
    fn columns_clamp_to_line_length() {
        let diags = [diag(0, 50, 0, 99)];
        let on = diagnostic_spans_on_line(&diags, 0, 5);
        assert_eq!((on[0].start, on[0].end), (5, 5)); // both clamped to EOL
    }

    #[test]
    fn zero_width_diagnostic_is_kept() {
        let diags = [diag(0, 2, 0, 2)];
        let on = diagnostic_spans_on_line(&diags, 0, 10);
        assert_eq!(on.len(), 1);
        assert_eq!((on[0].start, on[0].end), (2, 2));
    }

    #[test]
    fn navigate_diagnostic_finds_next_and_prev() {
        use DiagnosticDirection::{Next, Prev};
        // Diagnostics on lines 2 (col 4), 5, 9 — deliberately out of order to exercise the sort.
        let diags = [diag(5, 0, 5, 1), diag(2, 4, 2, 6), diag(9, 0, 9, 3)];
        // From line 3: next is line 5, prev is line 2 (at its column).
        assert_eq!(
            navigate_diagnostic_target(&diags, 3, Next),
            Some(LogicalPosition { line: 5, col: 0 })
        );
        assert_eq!(
            navigate_diagnostic_target(&diags, 3, Prev),
            Some(LogicalPosition { line: 2, col: 4 })
        );
        // Strictly beyond the cursor line: standing on a diagnostic line skips it.
        assert_eq!(
            navigate_diagnostic_target(&diags, 5, Next),
            Some(LogicalPosition { line: 9, col: 0 })
        );
        assert_eq!(
            navigate_diagnostic_target(&diags, 5, Prev),
            Some(LogicalPosition { line: 2, col: 4 })
        );
    }

    #[test]
    fn navigate_diagnostic_returns_none_at_the_ends() {
        use DiagnosticDirection::{Next, Prev};
        let diags = [diag(2, 0, 2, 1), diag(7, 0, 7, 1)];
        assert_eq!(navigate_diagnostic_target(&diags, 7, Next), None); // nothing past the last
        assert_eq!(navigate_diagnostic_target(&diags, 2, Prev), None); // nothing before the first
        assert_eq!(navigate_diagnostic_target(&[], 0, Next), None); // no diagnostics at all
    }
}

#[cfg(test)]
mod lsp_parse_tests {
    use super::*;
    use crate::lsp::position::PositionEncoding;
    use serde_json::json;

    #[test]
    fn hover_markup_content_string_and_array() {
        // MarkupContent markdown → text + markdown=true.
        assert_eq!(
            parse_hover_contents(&json!({"contents": {"kind": "markdown", "value": "fn foo()"}})),
            Some(("fn foo()".into(), true))
        );
        // MarkupContent plaintext → markdown=false (render literally).
        assert_eq!(
            parse_hover_contents(&json!({"contents": {"kind": "plaintext", "value": "a*b_c"}})),
            Some(("a*b_c".into(), false))
        );
        // A bare string is a MarkedString → markdown.
        assert_eq!(
            parse_hover_contents(&json!({"contents": "plain"})),
            Some(("plain".into(), true))
        );
        // Legacy MarkedString { language, value } → fenced as a markdown code block.
        assert_eq!(
            parse_hover_contents(&json!({"contents": {"language": "rust", "value": "let x = 1;"}})),
            Some(("```rust\nlet x = 1;\n```".into(), true))
        );
        // Array (MarkedString[]) → joined, markdown if any part is.
        assert_eq!(
            parse_hover_contents(&json!({"contents": [{"language": "rust", "value": "a"}, "b"]})),
            Some(("```rust\na\n```\n\nb".into(), true))
        );
    }

    #[test]
    fn hover_empty_or_absent_is_none() {
        assert!(parse_hover_contents(&json!({"contents": null})).is_none());
        assert!(parse_hover_contents(&json!({"contents": "   "})).is_none());
        assert!(parse_hover_contents(&json!({})).is_none());
    }

    #[test]
    fn definition_location_array_and_link() {
        // Bare Location.
        let v = json!({"uri": "file:///p/a.rs", "range": {"start": {"line": 3, "character": 5}, "end": {"line": 3, "character": 8}}});
        let loc = parse_definition(&v, PositionEncoding::Utf8).unwrap();
        assert_eq!(loc.path, "/p/a.rs");
        assert_eq!(loc.position, LogicalPosition { line: 3, col: 5 });
        // Array → first.
        let v = json!([{"uri": "file:///p/a.rs", "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 1}}}]);
        assert_eq!(
            parse_definition(&v, PositionEncoding::Utf8)
                .unwrap()
                .position
                .line,
            1
        );
        // LocationLink → targetSelectionRange preferred over targetRange.
        let v = json!([{
            "targetUri": "file:///p/b.rs",
            "targetSelectionRange": {"start": {"line": 7, "character": 2}, "end": {"line": 7, "character": 9}},
            "targetRange": {"start": {"line": 6, "character": 0}, "end": {"line": 8, "character": 0}}
        }]);
        let loc = parse_definition(&v, PositionEncoding::Utf8).unwrap();
        assert_eq!(loc.path, "/p/b.rs");
        assert_eq!(loc.position, LogicalPosition { line: 7, col: 2 });
    }

    #[test]
    fn definition_null_and_empty_is_none() {
        assert!(parse_definition(&json!(null), PositionEncoding::Utf8).is_none());
        assert!(parse_definition(&json!([]), PositionEncoding::Utf8).is_none());
    }

    #[test]
    fn references_parses_every_location() {
        // A `Location[]` with entries in two files — all are kept, in response order.
        let v = json!([
            {"uri": "file:///p/a.rs", "range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 9}}},
            {"uri": "file:///p/b.rs", "range": {"start": {"line": 4, "character": 8}, "end": {"line": 4, "character": 14}}},
        ]);
        let refs = parse_references(&v, PositionEncoding::Utf8);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].path, "/p/a.rs");
        assert_eq!(refs[0].position, LogicalPosition { line: 0, col: 3 });
        assert_eq!(refs[1].path, "/p/b.rs");
        assert_eq!(refs[1].position, LogicalPosition { line: 4, col: 8 });
    }

    #[test]
    fn references_null_and_non_array_is_empty() {
        // `textDocument/references` returns `Location[] | null`; both null and a stray object
        // yield no references rather than erroring.
        assert!(parse_references(&json!(null), PositionEncoding::Utf8).is_empty());
        assert!(
            parse_references(&json!({"uri": "file:///p/a.rs"}), PositionEncoding::Utf8).is_empty()
        );
        // Unparseable entries are skipped, not fatal.
        let v = json!([
            {"uri": "file:///p/a.rs", "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 1}}},
            {"garbage": true},
        ]);
        assert_eq!(parse_references(&v, PositionEncoding::Utf8).len(), 1);
    }

    #[test]
    fn document_symbols_flattens_hierarchy_with_depth() {
        // DocumentSymbol[]: a struct with two nested members. selectionRange drives the position;
        // children are flattened depth-first with incrementing depth.
        let v = json!([
            {
                "name": "Parser", "kind": 23, "detail": "struct Parser",
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 9, "character": 1}},
                "selectionRange": {"start": {"line": 0, "character": 7}, "end": {"line": 0, "character": 13}},
                "children": [
                    {
                        "name": "new", "kind": 6, "detail": "fn() -> Parser",
                        "range": {"start": {"line": 1, "character": 4}, "end": {"line": 3, "character": 5}},
                        "selectionRange": {"start": {"line": 1, "character": 11}, "end": {"line": 1, "character": 14}}
                    }
                ]
            }
        ]);
        let syms = parse_document_symbols(&v, "/p/a.rs", PositionEncoding::Utf8);
        assert_eq!(syms.len(), 2);
        assert_eq!(syms[0].name, "Parser");
        assert_eq!(syms[0].symbol_kind, aether_protocol::picker::SymbolKind::Struct);
        assert_eq!(syms[0].depth, 0);
        assert_eq!((syms[0].line, syms[0].col), (0, 7)); // selectionRange, not range
        assert_eq!(syms[0].detail, "struct Parser");
        assert_eq!(syms[1].name, "new");
        assert_eq!(syms[1].symbol_kind, aether_protocol::picker::SymbolKind::Method);
        assert_eq!(syms[1].depth, 1);
        assert_eq!((syms[1].line, syms[1].col), (1, 11));
        // The full `range` (not selectionRange) is captured for cursor containment: the struct
        // spans lines 0..9, so a cursor on line 5 falls inside it.
        assert_eq!(syms[0].range_start, LogicalPosition { line: 0, col: 0 });
        assert_eq!(syms[0].range_end, LogicalPosition { line: 9, col: 1 });
        assert!(syms[0].contains(LogicalPosition { line: 5, col: 0 }));
    }

    #[test]
    fn document_symbols_parses_flat_symbol_information() {
        // SymbolInformation[]: no selectionRange/children; position under location.range.
        // `containerName` is deliberately *not* used as detail (the tree indentation shows the
        // parent), so detail stays empty here.
        let v = json!([
            {
                "name": "helper", "kind": 12, "containerName": "mymod",
                "location": {"uri": "file:///p/a.rs", "range": {"start": {"line": 5, "character": 3}, "end": {"line": 5, "character": 9}}}
            }
        ]);
        let syms = parse_document_symbols(&v, "/p/a.rs", PositionEncoding::Utf8);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "helper");
        assert_eq!(syms[0].symbol_kind, aether_protocol::picker::SymbolKind::Function);
        assert_eq!(syms[0].detail, "", "containerName is not surfaced as detail");
        assert_eq!(syms[0].depth, 0);
        assert_eq!((syms[0].line, syms[0].col), (5, 3));
    }

    #[test]
    fn document_symbols_flat_reconstructs_depth_from_ranges() {
        // A flat SymbolInformation[] (like vscode-html) with nested ranges — html > head > meta —
        // gets its tree rebuilt from `range` containment so the outline indents.
        let loc = |s: (u64, u64), e: (u64, u64)| {
            json!({"range": {"start": {"line": s.0, "character": s.1}, "end": {"line": e.0, "character": e.1}}})
        };
        let v = json!([
            {"name": "html", "kind": 8, "location": loc((1, 0), (24, 7))},
            {"name": "head", "kind": 8, "location": loc((2, 2), (6, 9))},
            {"name": "meta", "kind": 8, "location": loc((3, 4), (3, 28))},
            {"name": "title", "kind": 8, "location": loc((4, 4), (4, 33))},
            {"name": "body", "kind": 8, "location": loc((7, 2), (23, 9))},
            {"name": "h1", "kind": 8, "location": loc((8, 4), (8, 20))},
        ]);
        let syms = parse_document_symbols(&v, "/p/a.html", PositionEncoding::Utf8);
        let depth = |name: &str| syms.iter().find(|c| c.name == name).unwrap().depth;
        assert_eq!(depth("html"), 0);
        assert_eq!(depth("head"), 1);
        assert_eq!(depth("body"), 1);
        assert_eq!(depth("meta"), 2);
        assert_eq!(depth("title"), 2);
        assert_eq!(depth("h1"), 2); // h1 under body
    }

    #[test]
    fn document_symbols_null_and_bad_entries_skipped() {
        // null / non-array → empty; entries missing name or position are skipped, not fatal.
        assert!(parse_document_symbols(&json!(null), "/p/a.rs", PositionEncoding::Utf8).is_empty());
        let v = json!([
            {"name": "ok", "kind": 13, "selectionRange": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}},
            {"kind": 13, "selectionRange": {"start": {"line": 1, "character": 0}}},
            {"name": "no_pos", "kind": 13},
        ]);
        let syms = parse_document_symbols(&v, "/p/a.rs", PositionEncoding::Utf8);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "ok");
    }

    #[test]
    fn apply_text_edits_single_and_multi() {
        use ropey::Rope;
        let text = Rope::from_str("foo\nbar\n");
        // Replace "bar" (line 1, cols 0..3) with "BAZ".
        let edits = json!([{"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 3}}, "newText": "BAZ"}]);
        assert_eq!(
            apply_lsp_text_edits(&text, &edits, PositionEncoding::Utf8).unwrap(),
            "foo\nBAZ\n"
        );
        // Two edits given out of order — descending-start application keeps offsets valid.
        let edits = json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "newText": "X"},
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 3}}, "newText": "Q"},
        ]);
        assert_eq!(
            apply_lsp_text_edits(&text, &edits, PositionEncoding::Utf8).unwrap(),
            "Xfoo\nQ\n"
        );
    }

    #[test]
    fn apply_text_edits_whole_document_and_empty() {
        use ropey::Rope;
        let text = Rope::from_str("a\nb\n");
        // Whole-document replace: end one past the last line clamps to the buffer end.
        let edits = json!([{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 2, "character": 0}}, "newText": "z\n"}]);
        assert_eq!(
            apply_lsp_text_edits(&text, &edits, PositionEncoding::Utf8).unwrap(),
            "z\n"
        );
        // Empty / absent edit lists are a no-op (None), not an empty document.
        assert!(apply_lsp_text_edits(&text, &json!([]), PositionEncoding::Utf8).is_none());
        assert!(apply_lsp_text_edits(&text, &json!(null), PositionEncoding::Utf8).is_none());
    }
}
