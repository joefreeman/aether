//! `workspace/*` — activation, roots and projects, worktree binding, rename/delete, and the
//! session persistence and dormant-buffer restore that hang off them. Also `history/*`, the
//! per-workspace overlay-input recall lists.

use super::*;

/// The active workspace's input-history lists. Deliberately *not* an error without an active
/// workspace: the boot chooser fetches this on connect before any workspace exists, and an
/// ephemeral ("(no workspace)") context has no stable key to file history under — both get empty
/// lists, which the client treats as "nothing to recall".
pub async fn history_state(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    _params: HistoryStateParams,
) -> Result<HistoryStateResult, RpcError> {
    let s = state.lock().await;
    let lists = match history_workspace(&s, ctx.client_id) {
        Some(name) => s.history.lists(&name),
        None => Default::default(),
    };
    Ok(HistoryStateResult { lists })
}

/// Append one committed value to the active workspace's list. Silently a no-op without a
/// persistable workspace (same rule as [`history_state`]) and for a value the shared
/// dedupe/cap rule rejects; the write to `history.json` rides the periodic flush
/// ([`flush_history`]), not this request.
pub async fn history_record(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: HistoryRecordParams,
) -> Result<HistoryRecordResult, RpcError> {
    let mut s = state.lock().await;
    let Some(workspace) = history_workspace(&s, ctx.client_id) else {
        return Ok(HistoryRecordResult {});
    };
    if s.history.record(&workspace, params.kind, params.entry) {
        s.history_dirty = true;
    }
    Ok(HistoryRecordResult {})
}

/// The workspace name to file this client's history under, or `None` when there's nothing
/// persistable: no workspace active yet, or an ephemeral one (a synthesized context for a file
/// outside every workspace — its id is minted per run, so history filed under it could never be
/// recalled). [`WorkspaceEntry::name`] already draws that line: `Some` ⇔ a `<name>.toml` on disk.
fn history_workspace(s: &ServerState, client_id: ClientId) -> Option<String> {
    s.active_workspace(client_id)?.name.clone()
}

/// Write the input-history lists to disk when they changed since the last flush. Same contract as
/// [`flush_hints`] — periodic dirty-flag debounce plus a final flush on graceful shutdown,
/// best-effort, write off the lock.
pub(crate) async fn flush_history(state: &SharedState) {
    let (path, snapshot) = {
        let mut s = state.lock().await;
        let Some(path) = s.history_path.clone() else {
            return;
        };
        if !s.history_dirty {
            return;
        }
        s.history_dirty = false;
        (path, s.history.clone())
    };
    if let Err(e) = crate::config::write_history_at(&path, &snapshot) {
        tracing::warn!(error = %e, "failed to write input history");
    }
}

/// Activate a workspace for this client. Loads the workspace's config from disk if no client has it
/// active yet (lazy load). If the client already has a different workspace active, tears down the
/// client's per-buffer state for the prior workspace before switching. Returns the resolved
/// workspace info (name + canonical paths) so the client can present buffers relative to those
/// roots. The client-facing view of a workspace's declared projects, re-resolved against its roots
/// on every read.
///
/// Deliberately not cached: resolution depends on the filesystem, so a project whose directory was
/// deleted by a branch switch starts reporting an error, and one that comes back stops — with no
/// reactivation and no invalidation to remember. An entry that fails still appears, carrying its
/// reason, because a broken declaration the user can see and fix beats one that silently vanishes.
pub fn workspace_project_views(entry: &crate::state::WorkspaceEntry) -> Vec<WorkspaceProject> {
    entry
        .projects
        .iter()
        .map(|p| {
            let (path_index, relative_path) = (p.root_index, p.relative_path.display().to_string());
            match crate::config::resolve_project(p, &entry.paths) {
                Ok(r) => WorkspaceProject {
                    path_index,
                    relative_path,
                    language: r.language,
                    error: None,
                },
                Err(error) => WorkspaceProject {
                    path_index,
                    relative_path,
                    language: p.language.clone().unwrap_or_default(),
                    error: Some(error),
                },
            }
        })
        .collect()
}

/// A language server to start: its key, how to launch it, and the generation the handle was created
/// with. Collected under the state lock and spawned after it drops.
pub(crate) type PinnedLaunch = (
    crate::lsp::manager::LspServerKey,
    crate::lsp::config::LspServerSpec,
    u64,
);

/// Reconcile a workspace's pinned language servers with the projects it currently declares.
///
/// Pins every declared project's server — creating the handle if it isn't running — and drops the
/// pin from any server the workspace no longer declares, reaping it unless buffers are open against
/// it. Projects that fail to resolve are logged and skipped; a broken declaration must not stop the
/// rest from starting.
///
/// Idempotent, which is what lets activation, `add_project` and `remove_project` all just call it
/// and let it work out the difference. Returns the launches for the caller to spawn once the lock
/// is released — a handshake plus initial indexing takes seconds and must not be held under it.
pub(crate) fn reconcile_workspace_pins(
    s: &mut ServerState,
    workspace_id: &str,
) -> Vec<PinnedLaunch> {
    let Some(entry) = s.workspaces.get(workspace_id) else {
        return Vec::new();
    };
    let roots = entry.paths.clone();
    let declared = entry.projects.clone();

    let mut launches = Vec::new();
    let mut wanted: std::collections::HashSet<crate::lsp::manager::LspServerKey> =
        std::collections::HashSet::new();
    for e in &declared {
        let resolved = match crate::config::resolve_project(e, &roots) {
            Ok(r) => r,
            Err(err) => {
                tracing::warn!(workspace = %workspace_id, error = %err, "skipping project");
                continue;
            }
        };
        // `resolve_project` already established the language has a spec.
        let Some(spec) = crate::lsp::config::server_spec(&resolved.language) else {
            continue;
        };
        let key = crate::lsp::manager::LspServerKey::new(resolved.root, &resolved.language);
        // `ensure` yields a generation only for a *fresh* handle; one already running (lazily
        // launched by a buffer, or shared with a sibling project in the same root) just needs the
        // pin.
        if let Some(generation) = s.lsp.ensure(&key, spec.command) {
            launches.push((key.clone(), spec, generation));
        }
        s.lsp.pin(&key, workspace_id);
        wanted.insert(key);
    }

    // Anything pinned by this workspace but no longer declared reverts to the ordinary
    // reap-on-last-buffer lifetime, and goes now if it has no buffers.
    let stale: Vec<crate::lsp::manager::LspServerKey> = s
        .lsp
        .servers
        .iter()
        .filter(|(k, h)| h.pinned_by.contains(workspace_id) && !wanted.contains(*k))
        .map(|(k, _)| k.clone())
        .collect();
    for key in stale {
        if let Some(h) = s.lsp.servers.get_mut(&key) {
            h.pinned_by.remove(workspace_id);
            // Only reap once *nobody* pins it: another context declaring the same project keeps
            // the same process, and undeclaring here must not pull it out from under them.
            if h.pinned_by.is_empty()
                && h.open_documents.is_empty()
                && h.registered_buffers.is_empty()
            {
                s.lsp.servers.remove(&key);
            }
        }
    }
    launches
}

/// [`workspace_project_views`] for a workspace by id, or empty when it isn't loaded.
fn workspace_project_views_by_id(s: &ServerState, workspace_id: &str) -> Vec<WorkspaceProject> {
    s.workspaces
        .get(workspace_id)
        .map(workspace_project_views)
        .unwrap_or_default()
}

pub async fn workspace_activate(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: WorkspaceActivateParams,
) -> Result<WorkspaceActivateResult, RpcError> {
    let sessions_path = state.lock().await.sessions_path.clone();
    // Which **context** of this workspace to enter. A context is `(workspace, bindings)` and the
    // base is the empty set, so resolving it here is the only place that decision is made — every
    // base-versus-bound branch the old shape needed collapses into this one line.
    let requested = params.worktrees.as_ref().map(normalise_bindings);
    let bindings = worktree_bindings(sessions_path.as_deref(), &params.name, requested.as_ref());
    activate_context(state, ctx, params.name, bindings, params.open_last).await
}

/// Activate a context whose bindings are already resolved and normalised onto repo families.
///
/// Split from [`workspace_activate`] so the callers that compute a binding set themselves — binding
/// a worktree, which mutates one entry of the current set — can reach it without round-tripping
/// their families back through `RepoId`s just to have them normalised again. That round trip is not
/// merely wasteful: recovering a `RepoId` from a family means `common_dir.parent`, which is not
/// the main worktree for a bare or `--separate-git-dir` repo, so a binding would be silently
/// dropped in exactly the cases hardest to notice.
pub async fn activate_context(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    name: String,
    bindings: std::collections::BTreeMap<std::path::PathBuf, String>,
    open_last: bool,
) -> Result<WorkspaceActivateResult, RpcError> {
    let params = WorkspaceActivateParams {
        name,
        worktrees: None,
        open_last,
    };
    let started = std::time::Instant::now();
    let client_id = ctx.client_id;

    // Cold path: read the workspace's config from disk *outside* the state lock — file I/O and
    // canonicalization can be slow on cold caches, and we hold the lock for many concurrent
    // operations.
    //
    // A workspace is its **configured roots** (the TOML, or an in-memory registration) plus its
    // **worktree bindings** (the session file). The two live apart deliberately: a binding that no
    // longer resolves is stale machine state, dropped on load, where a stale root would be a broken
    // config file to explain. Which is also why binding can never strand you — the configured roots
    // are always still there to fall back to.
    //
    // "Is it loaded?" is asked **once**, here. It used to be asked twice under two separate locks —
    // The id derived from the bindings is what makes a second client with the same worktrees
    // *join* this entry rather than create a rival one — which is why "is this worktree already
    // open somewhere?" is a map lookup here and not a case to handle.
    let context = crate::worktree::context_id(&params.name, &bindings);

    // Two questions, asked **once** each, under one lock: is *this context* loaded, and — when it
    // isn't — is any **sibling** context of the same workspace? A sibling already holds the
    // workspace's configured roots and projects, so a new context of a workspace that is open
    // elsewhere is built from those rather than re-read from disk.
    //
    // Not merely an optimisation. A workspace can be registered purely in memory (an embedding, a
    // test) with no TOML at all; going to disk for a *second* context of one would fail an
    // activation that has everything it needs already in hand. It is also what keeps siblings
    // consistent: `configured_paths` is the one shape a root edit updates, so a context created
    // after one inherits it.
    let (loaded, sibling) = {
        let s = state.lock().await;
        let shape =
            |e: &crate::state::WorkspaceEntry| (e.configured_paths().to_vec(), e.projects.clone());
        let loaded = s.workspaces.get(&context).map(shape);
        let sibling = match &loaded {
            Some(_) => None,
            None => s
                .workspaces
                .values()
                .find(|e| e.name.as_deref() == Some(params.name.as_str()))
                .map(shape),
        };
        (loaded, sibling)
    };
    let already_loaded = loaded.is_some();

    let cold_load: Option<ColdLoad> = if already_loaded {
        None
    } else if let Some((configured, projects)) = sibling {
        let (roots, base_paths) = materialise(&params.name, configured, &bindings, true).await?;
        Some((params.name.clone(), roots, projects, base_paths))
    } else {
        let workspaces_dir = state
            .lock()
            .await
            .workspaces_dir()
            .map_err(|e| RpcError::internal(format!("resolving workspaces dir: {e}")))?;
        let cfg = match crate::config::load_workspace_in(&workspaces_dir, &params.name) {
            Ok(c) => c,
            // A config that isn't there and one that won't parse are different problems: the first
            // is a wrong name, the second a broken (or stale-format) file the user has to go and
            // fix. Reporting both as "unknown workspace" sends them hunting for a missing file.
            Err(crate::config::LoadWorkspaceError::NotFound) => {
                tracing::warn!(name = %params.name, "workspace/activate: no such workspace");
                return Err(RpcError::unknown_workspace(&params.name));
            }
            Err(crate::config::LoadWorkspaceError::Invalid(e)) => {
                tracing::warn!(name = %params.name, error = %e, "workspace/activate: unreadable config");
                return Err(RpcError::invalid_params(e));
            }
        };
        let configured: Vec<std::path::PathBuf> = cfg
            .paths()
            .iter()
            .map(|p| crate::config::canonicalize_workspace_path(p))
            .collect::<Result<_, _>>()
            .map_err(|e| RpcError::invalid_path(format!("canonicalizing workspace path: {e}")))?;
        let projects = cfg.project_refs();
        let (roots, base_paths) = materialise(&cfg.name, configured, &bindings, true).await?;
        Some((cfg.name, roots, projects, base_paths))
    };

    // An already-loaded *bound* workspace re-materialises before we go any further. Its roots are
    // derived from bindings and from what those bindings resolve to, and both can have moved since
    // it was loaded — another client rebound it, or a `git worktree remove` in a terminal made a
    // binding dangle. Taking the hot path unconditionally would re-enter a workspace pointing at a
    // directory that is no longer there.
    if already_loaded && !bindings.is_empty() {
        let rebound = rebind_loaded_workspace(state, client_id, &context, None).await?;
        if !rebound.stayed.is_empty() {
            // Logged, not reported: the buffers are still open at their old paths, and the
            // switcher's per-workspace unsaved dot is the standing answer to "where did my edits
            // go".
            tracing::info!(
                workspace = %context,
                dirty_left_behind = rebound.stayed.len(),
                "unsaved buffers stayed on the previous tree"
            );
        }
    }

    let mut s = state.lock().await;

    // If the client had a different workspace active, tear down its prior per-buffer state.
    let prior = s
        .clients
        .get(&client_id)
        .and_then(|c| c.active_workspace.clone());
    if let Some(prior_name) = &prior {
        if prior_name != &context {
            s.teardown_client_state_for_workspace(client_id, prior_name);
        }
    }

    // The reserved views of restored buffers that carry unsaved content (a backup) and so should
    // be materialized eagerly after the lock is released — see the restore block below and the
    // loop after it.
    let mut eager_restore_ids: Vec<ViewId> = Vec::new();

    // Watch registration deferred until after the lock is released — see below.
    let mut watch_after: Option<(Arc<crate::watcher::WatcherHandle>, Vec<std::path::PathBuf>)> =
        None;

    // Install the workspace entry on the cold path. Reuse the existing entry (and its shared
    // `WorkspaceIndex`) on the hot path.
    if let Some((name, canonical_paths, projects, base_paths)) = cold_load {
        let workspace_index = Arc::new(crate::workspace_index::WorkspaceIndex::new(
            canonical_paths.clone(),
        ));
        s.workspaces.insert(
            context.clone(),
            crate::state::WorkspaceEntry {
                id: context.clone(),
                name: Some(name),
                base_paths,
                paths: canonical_paths.clone(),
                workspace_index,
                worktrees: bindings.clone(),
                mru_views: std::collections::VecDeque::new(),
                dormant_views: Vec::new(),
                jumplist: None,
                projects,
            },
        );
        // Hand the new roots to the watcher so its events flow for this workspace too — but only
        // after the lock is released, on a blocking task: registration walks the roots (ignore-
        // filtered, but still disk I/O — slow on a cold cache), and activation shouldn't wait for
        // it. The gap is benign: an external change in the first moments simply goes unnoticed,
        // exactly as one landing just before registration always did. Best-effort — a watcher
        // failure shouldn't refuse activation. `watcher` is `None` only when the server skipped
        // initializing it (failed at startup); we just skip registration in that case.
        watch_after = s.watcher.clone().map(|w| (w, canonical_paths.clone()));

        // First activation this session: restore the workspace's previously-open buffers from the
        // persisted session. Clean files become *dormant* (listed in the picker, loaded lazily — a
        // reserved id, no rope/LSP until materialized). Buffers carrying unsaved content (those with
        // a backup) are instead materialized *eagerly* (below, after the lock) so they come back as
        // live, dirty buffers rather than clean-looking dormant rows. Files deleted while the server was down with no
        // backup are dropped. No-op when sessions aren't persisted (`sessions_path` unset).
        if let Some(path) = s.sessions_path.clone() {
            if let Ok(sessions) = crate::config::load_workspace_sessions_at(&path) {
                // Keyed by workspace *name* then bindings, not by the context id: the session file
                // nests contexts under their workspace rather than inventing a composite key.
                let entries = sessions
                    .workspaces
                    .get(&params.name)
                    .map(|sess| sess.views_for(&bindings))
                    .unwrap_or(&[]);
                let sources = restore_dormant_sources(entries, &context, s.backups_path.as_deref());
                let dormant: Vec<crate::state::DormantView> = sources
                    .into_iter()
                    .map(|(source, kind)| {
                        let id = s.allocate_buffer_id();
                        crate::state::DormantView {
                            id,
                            view: s.allocate_view_id(),
                            kind,
                            source,
                        }
                    })
                    .collect();
                // Which restored entries carry unsaved content (a backup) → eager-materialize.
                let backups_root = s.backups_path.clone();
                eager_restore_ids = dormant
                    .iter()
                    .filter(|d| match &d.source {
                        crate::state::DormantSource::Scratch { .. } => true,
                        crate::state::DormantSource::File(p) => {
                            backups_root.as_deref().is_some_and(|root| {
                                crate::backup::exists(&crate::backup::file_backup_path(root, p))
                            })
                        }
                        // A revision is read-only, so it can never hold unsaved content to rescue;
                        // it regenerates when first viewed, like a dormant file with no backup.
                        crate::state::DormantSource::Virtual { .. } => false,
                    })
                    .map(|d| d.view)
                    .collect();
                if let Some(proj) = s.workspaces.get_mut(&context) {
                    proj.dormant_views = dormant;
                }
            }
        }
    }

    // Start (or re-start) the workspace's declared projects. Deliberately on *every* activation,
    // not just the cold load: the daemon outlives its clients, so a client quitting released the
    // pins (`unpin_workspace_if_unused`) while leaving the workspace loaded — and the next
    // activation would then take the already-loaded path and never bring them back. Reconcile is
    // idempotent (`ensure` no-ops for a running server), so an activation that changes nothing
    // costs nothing.
    // Handles are created (and pinned) under the lock; the handshakes are spawned after it drops,
    // so activation never waits on N server startups.
    let pinned_launches: Vec<PinnedLaunch> = reconcile_workspace_pins(&mut s, &context);

    let entry_paths: Vec<String> = s
        .workspaces
        .get(&context)
        .map(|p| p.paths.iter().map(|p| p.display().to_string()).collect())
        .unwrap_or_default();
    let entry_projects = workspace_project_views_by_id(&s, &context);
    let server_started_at = s.started_at_unix_ms;

    if let Some(session) = s.clients.get_mut(&client_id) {
        session.active_workspace = Some(context.clone());
    }

    // Switching away from an ephemeral workspace can leave it empty (its transient buffers were
    // closed by the teardown above; any permanent ones keep it alive). Retire it now that this
    // client no longer holds it active, so a throwaway "(no workspace)" context doesn't linger.
    let mut unpin_pushes = Vec::new();
    if let Some(prior_id) = &prior {
        if prior_id != &context {
            s.prune_ephemeral_if_empty(prior_id);
            // This client left the old workspace; if it was the last one there, its pinned project
            // servers have no reason to stay up.
            unpin_pushes = unpin_workspace_if_unused(&mut s, prior_id);
        }
    }

    let last_view_id = landing_view_id(&s, &context);

    tracing::info!(
        %client_id,
        workspace = %context,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "client activated workspace"
    );
    drop(s);

    for (sender, notif) in unpin_pushes {
        let _ = sender.send(notif).await;
    }

    if let Some((w, roots)) = watch_after {
        tokio::task::spawn_blocking(move || crate::watcher::watch_workspace_paths(&w, &roots));
    }

    // Start the declared projects' servers. Detached, like the watcher registration above: a
    // rust-analyzer handshake plus its initial indexing takes seconds, and the point of pinning is
    // that it happens *while* you read code, not before you can.
    for (key, spec, generation) in pinned_launches {
        tokio::spawn(crate::lsp::manager::launch(
            state.clone(),
            key,
            spec,
            generation,
        ));
    }

    // Composite post-step: open the landing buffer — the workspace's MRU buffer, or a fresh
    // transient scratch on a first visit — in the same round-trip. Mirrors the convention every
    // client implemented by hand.
    let opened = if params.open_last {
        Some(
            view_open(
                state,
                ctx,
                ViewOpenParams {
                    view_id: last_view_id,
                    transient: if last_view_id.is_none() {
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

    // Eagerly materialize the restored buffers that carry unsaved content, so they come back as
    // live, dirty buffers (marked unsaved in the picker) rather than clean-looking dormant
    // rows — the hot-exit promise. Each is opened by its reserved view through the normal open
    // path, which loads the file (or rebuilds the scratch) and overlays the backup. The landing
    // view above may have already materialized one of them; skip it. Best-effort: a single failure
    // mustn't abort activation (e.g. the rare unreadable file — its content still survives in the
    // backup).
    for view in eager_restore_ids {
        if params.open_last && Some(view) == last_view_id {
            continue;
        }
        let _ = view_open(
            state,
            ctx,
            ViewOpenParams {
                view_id: Some(view),
                ..Default::default()
            },
        )
        .await;
    }

    // Stamp this activation and refresh the persisted buffer list (now that the landing buffer has
    // materialized). Drives the switcher's recency ordering on the next open.
    persist_workspace_session(state, &context, true).await;

    Ok(WorkspaceActivateResult {
        workspace: WorkspaceInfo {
            name: params.name,
            paths: entry_paths,
            worktrees: wire_bindings(&bindings),
            projects: entry_projects,
        },
        last_view_id,
        opened,
        server_started_at,
    })
}

/// The view a client lands on when it arrives in `context` with nothing specific to open — a
/// workspace switch, or a directory opened as a temporary context. The MRU lives on
/// `WorkspaceEntry` (not per-client) so it survives client disconnects: a fresh invocation sees the
/// same top-of-MRU view the prior session left there, and reattaches instead of spawning a fresh
/// scratch on every switch.
///
/// Prefer a still-live MRU view; otherwise — a cold restore after a restart, where nothing is
/// loaded yet — the most-recently-used *dormant* row's reserved view, which `view_open`
/// materializes. `None` only when the workspace is genuinely empty (a first ever visit, and always
/// the case for a freshly minted temporary one), which is the caller's cue to mint a transient
/// scratch.
///
/// Deliberately kind-blind: a scratch you were editing is where you left off, and coming back to it
/// is the point. A fresh scratch is only ever minted when there is no view of any kind to return
/// to — a state with no file to prefer — so this never opens a blank scratch over a file.
fn landing_view_id(s: &ServerState, context: &str) -> Option<ViewId> {
    s.mru_view(context)
        .or_else(|| s.first_dormant_view(context))
}

/// Persist `workspace_name`'s session — the canonical paths of its open (and still-dormant) buffers,
/// most-recently-used first, and optionally a fresh `last_activated_at` stamp — to the session file
/// ([`crate::config::WorkspaceSessions`]). Best-effort: a no-op when sessions aren't persisted
/// (`sessions_path` unset) or the workspace is ephemeral, and it logs rather than fails on I/O error.
/// Buffer paths are gathered under the lock; the read-modify-write of the file happens after it's
/// released.
pub async fn persist_workspace_session(
    state: &SharedState,
    workspace_name: &str,
    touch_activation: bool,
) {
    // Hold the state lock across the whole read-modify-write. It's the single server-wide
    // serialization point, so this makes concurrent persists — and the recency-sort read in
    // `workspace_candidates`, which also runs under the lock — mutually exclusive: no lost updates
    // and no torn file. The session file is tiny, so the blocking I/O held under the lock is
    // sub-millisecond, and persists are human-paced (open / switch / save / close / activate).
    // No `.await` between here and the write, so the guard is never yielded mid-critical-section.
    let s = state.lock().await;
    let Some(path) = s.sessions_path.clone() else {
        return;
    };
    // Skip ephemeral workspaces (no `<name>.toml`): nothing to persist for a throwaway context.
    let Some((name, bindings)) = s
        .workspaces
        .get(workspace_name)
        .and_then(|e| Some((e.name.clone()?, e.worktrees.clone())))
    else {
        return;
    };
    let buffers = s.session_views(workspace_name);
    let mut sessions = crate::config::load_workspace_sessions_at(&path).unwrap_or_default();
    let entry = sessions.workspaces.entry(name).or_default();
    // One context's buffers, recorded against the bindings that identify it — so two windows in two
    // trees of one repo keep two buffer lists rather than overwriting each other's.
    if touch_activation {
        entry.record(&bindings, buffers, crate::config::now_unix_ms());
    } else {
        let at = entry
            .contexts
            .iter()
            .find(|c| c.worktrees == bindings)
            .map_or(entry.last_activated_at, |c| c.last_activated_at);
        entry.record(&bindings, buffers, at);
    }
    if let Err(e) = crate::config::write_workspace_sessions_at(&path, &sessions) {
        tracing::warn!(workspace = %workspace_name, error = %e, "failed to persist workspace session");
    }
}

/// Decide what to restore as dormant buffers for `workspace_name`, given its persisted session
/// `entries` and the backups root (`None` when backups are disabled). Pure except for filesystem
/// existence checks, so it's unit-testable against a tempdir.
///
/// - A **file** is restored while it still exists on disk, or while it has a backup (a file deleted
///   externally with unsaved content still comes back, flagged externally-deleted at materialize).
/// - A **scratch** is restored only if its unsaved content survives as a backup — first from its
///   session entry, then by scanning the scratch backup dir for any number the session didn't record
///   (a dirty scratch whose session write didn't land before shutdown). Files can't be scanned that
///   way: their backup filename is an unreversible path hash, so an unrecorded file backup relies on
///   recover-on-open instead. Order: session entries (MRU) first, then recovered scratches ascending.
fn restore_dormant_sources(
    entries: &[crate::config::SessionView],
    workspace_name: &str,
    backups_root: Option<&std::path::Path>,
) -> Vec<(
    crate::state::DormantSource,
    Option<aether_protocol::ui::ViewKind>,
)> {
    use crate::config::SessionView;
    use crate::state::DormantSource;
    let mut sources: Vec<(DormantSource, Option<aether_protocol::ui::ViewKind>)> = entries
        .iter()
        .filter_map(|entry| match entry {
            SessionView::Editor { .. } | SessionView::Reader { .. } | SessionView::File { .. } => {
                let (path, kind) = entry.file_view()?;
                let has_backup = backups_root.is_some_and(|root| {
                    crate::backup::exists(&crate::backup::file_backup_path(root, path))
                });
                (path.exists() || has_backup)
                    .then(|| (DormantSource::File(path.to_path_buf()), Some(kind)))
            }
            SessionView::Scratch { number } => {
                let has_backup = backups_root.is_some_and(|root| {
                    crate::backup::exists(&crate::backup::scratch_backup_path(
                        root,
                        workspace_name,
                        *number,
                    ))
                });
                has_backup.then_some((DormantSource::Scratch { number: *number }, None))
            }
            // Nothing on disk to check for: a revision is regenerated from the repo on first view,
            // and one that no longer resolves reports itself then rather than being probed here.
            SessionView::Virtual { key } => {
                Some((DormantSource::Virtual { key: key.clone() }, None))
            }
        })
        .collect();
    if let Some(root) = backups_root {
        let known: std::collections::HashSet<u32> = sources
            .iter()
            .filter_map(|(src, _)| match src {
                DormantSource::Scratch { number } => Some(*number),
                DormantSource::File(_) | DormantSource::Virtual { .. } => None,
            })
            .collect();
        if let Ok(dir) = std::fs::read_dir(root.join("scratch").join(workspace_name)) {
            let mut recovered: Vec<u32> = dir
                .flatten()
                .filter_map(|e| e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()))
                .filter(|n| !known.contains(n))
                .collect();
            recovered.sort_unstable();
            sources.extend(
                recovered
                    .into_iter()
                    .map(|number| (DormantSource::Scratch { number }, None)),
            );
        }
    }
    sources
}

/// Delete the on-disk backups this buffer's save/close resolves. A buffer can in principle have
/// both keys live at once (a scratch that was saved-as keeps its number *and* gains a path), so
/// both are considered. The scratch backup (per-workspace, never shared) always goes. The
/// document-level file backup goes only when the document is clean or this is its last
/// attachment — closing one workspace's view of a shared *dirty* document must not discard the
/// content's backup out from under the surviving workspaces. Called at the explicit "this content
/// is resolved" edges: save and close. No-op when backups aren't enabled.
pub fn delete_buffer_backups(s: &ServerState, workspace: &str, buf: &Buffer, doc: &Document) {
    let Some(root) = s.backups_path.as_deref() else {
        return;
    };
    if let Some(p) = doc.canonical_path.as_deref() {
        let has_sibling = s
            .buffers
            .values()
            .any(|o| o.document == doc.id && o.id != buf.id);
        if !doc.dirty || !has_sibling {
            crate::backup::delete(&crate::backup::file_backup_path(root, p));
        }
    }
    if let Some(n) = buf.scratch_number {
        crate::backup::delete(&crate::backup::scratch_backup_path(root, workspace, n));
    }
}

/// Flush unsaved-document backups to disk: write a backup for every dirty document with at least
/// one attachment in a *named* workspace, once its content changed since its last backup, and
/// delete the backup for any document that's gone clean again (e.g. undone back to the saved
/// state). One write per document regardless of how many workspaces hold it — the backup is a
/// property of the shared content, not of any workspace's view. This is the single writer of backup
/// files — deliberately edit-source agnostic, so it captures every kind of edit (typing, format,
/// revert, surround, …) without hooking each site. Best-effort: a no-op when backups aren't enabled
/// (`backups_path` unset); logs rather than fails on I/O error. The (potentially large) rope clones
/// and the file I/O happen off the lock; only the cheap `backed_up_revision` stamp is taken under
/// it.
pub(crate) async fn flush_backups(state: &SharedState) {
    enum Action {
        Write(String, aether_protocol::Revision),
        Delete,
    }
    struct Job {
        doc: DocumentId,
        path: std::path::PathBuf,
        action: Action,
    }
    let jobs: Vec<Job> = {
        let s = state.lock().await;
        let Some(root) = s.backups_path.clone() else {
            return;
        };
        let mut jobs = Vec::new();
        for doc in s.documents.values() {
            // Every *file-backed* document is backup-worthy, whatever kind of workspace holds
            // it: the backup key is path-only (`files/<hash>`) and recover-on-open is
            // workspace-agnostic, so content edited through an ephemeral tether context is
            // restored the next time the file is opened from anywhere — the ephemeral workspace
            // being gone by then doesn't matter. A *scratch* still needs a named-workspace
            // attachment: its backup keys on `scratch/<workspace>/<number>`, and an ephemeral
            // workspace id is minted per open and never looked up again.
            let path = if let Some(p) = doc.canonical_path.as_deref() {
                crate::backup::file_backup_path(&root, p)
            } else {
                let named_scratch = s.buffers.values().find_map(|b| {
                    let n = b.scratch_number?;
                    (b.document == doc.id).then_some(())?;
                    let workspace = s.buffer_workspaces.get(&b.id)?;
                    s.workspaces
                        .get(workspace)
                        .is_some_and(|w| w.name.is_some())
                        .then(|| crate::backup::scratch_backup_path(&root, workspace, n))
                });
                let Some(path) = named_scratch else {
                    continue;
                };
                path
            };
            if doc.dirty {
                if doc.backed_up_revision != Some(doc.revision) {
                    let content: String = doc.text.chunks().collect();
                    jobs.push(Job {
                        doc: doc.id,
                        path,
                        action: Action::Write(content, doc.revision),
                    });
                }
            } else if doc.backed_up_revision.is_some() {
                jobs.push(Job {
                    doc: doc.id,
                    path,
                    action: Action::Delete,
                });
            }
        }
        jobs
    };
    if jobs.is_empty() {
        return;
    }
    let mut stamps: Vec<(DocumentId, Option<aether_protocol::Revision>)> = Vec::new();
    for job in jobs {
        match job.action {
            Action::Write(content, rev) => match crate::backup::write(&job.path, &content) {
                Ok(()) => stamps.push((job.doc, Some(rev))),
                Err(e) => {
                    tracing::warn!(document = job.doc.0, error = %e, "failed to write document backup")
                }
            },
            Action::Delete => {
                crate::backup::delete(&job.path);
                stamps.push((job.doc, None));
            }
        }
    }
    let mut s = state.lock().await;
    for (doc_id, stamp) in stamps {
        if let Some(doc) = s.documents.get_mut(&doc_id) {
            // Stamping a revision the document may have already moved past is fine: the next
            // flush sees `revision != backed_up_revision` and rewrites.
            doc.backed_up_revision = stamp;
        }
    }
}

/// Validate and normalize a user-supplied workspace name. Trims surrounding whitespace, then
/// rejects empty names and names containing path separators — the name becomes a `<name>.toml`
/// filename, so a `/`, `\`, `.`, or `..` could escape the workspaces dir. Shared by
/// `workspace/create` and `workspace/rename`.
///
/// Also rejects the single reserved word `ephemeral`. Workspace ids namespace on the separator —
/// `aether`, `aether/feature-auth` (a worktree variant), `ephemeral/3` (a no-workspace context) —
/// so a workspace actually *called* `ephemeral` would give its variants ids that
/// [`aether_protocol::is_ephemeral_workspace_id`] reads as throwaway contexts. That misreading is
/// silent, and it costs a persisted workspace its session, so the collision is refused at the one
/// place a name is chosen.
fn validate_workspace_name(raw: &str) -> Result<String, RpcError> {
    let name = raw.trim().to_string();
    if name.is_empty() {
        return Err(RpcError::invalid_params("workspace name must not be empty"));
    }
    if name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        return Err(RpcError::invalid_params(
            "workspace name must not contain path separators",
        ));
    }
    if name == aether_protocol::RESERVED_WORKSPACE_NAME {
        return Err(RpcError::invalid_params(format!(
            "\"{name}\" is a reserved workspace name",
        )));
    }
    Ok(name)
}

/// Create a fresh workspace with no roots. Writes an empty-`paths` TOML to disk, registers the
/// workspace in memory, and activates it for the calling client. Refuses if a workspace of that
/// name already exists, or if the name is empty / contains path separators.
pub async fn workspace_create(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: WorkspaceCreateParams,
) -> Result<WorkspaceActivateResult, RpcError> {
    let client_id = ctx.client_id;
    let name = validate_workspace_name(&params.name)?;
    let workspaces_dir = state
        .lock()
        .await
        .workspaces_dir()
        .map_err(|e| RpcError::internal(format!("resolving workspaces dir: {e}")))?;
    let exists = crate::config::workspace_config_exists_in(&workspaces_dir, &name);
    if exists {
        return Err(RpcError::invalid_params(format!(
            "workspace {name} already exists"
        )));
    }
    // Write the TOML outside the state lock — file I/O.
    crate::config::write_workspace_config_in(
        &workspaces_dir,
        &crate::config::WorkspaceConfig {
            name: name.clone(),
            roots: Vec::new(),
        },
    )
    .map_err(|e| RpcError::internal(format!("writing workspace config: {e}")))?;

    let mut s = state.lock().await;

    // Tear down the client's prior workspace state (same flow as workspace_activate).
    let prior = s
        .clients
        .get(&client_id)
        .and_then(|c| c.active_workspace.clone());
    if let Some(prior_name) = &prior {
        if prior_name != &name {
            s.teardown_client_state_for_workspace(client_id, prior_name);
        }
    }

    // Register the empty workspace. No paths → no workspace_index walk to do; the empty Arc
    // returns an empty file list on access.
    let workspace_index = Arc::new(crate::workspace_index::WorkspaceIndex::new(Vec::new()));
    s.workspaces.insert(
        name.clone(),
        crate::state::WorkspaceEntry {
            worktrees: Default::default(),
            id: name.clone(),
            name: Some(name.clone()),
            base_paths: None,
            paths: Vec::new(),
            // A fresh workspace has no roots, so it reaches no repo and can bind nothing. Its id is
            // its name, which is what `context_id` gives for the empty set.
            workspace_index,
            mru_views: std::collections::VecDeque::new(),
            dormant_views: Vec::new(),
            jumplist: None,
            projects: Vec::new(),
        },
    );
    if let Some(session) = s.clients.get_mut(&client_id) {
        session.active_workspace = Some(name.clone());
    }
    // Creating a workspace while parked in an ephemeral one retires the ephemeral if now empty.
    let mut unpin_pushes = Vec::new();
    if let Some(prior_id) = &prior {
        if prior_id != &name {
            s.prune_ephemeral_if_empty(prior_id);
            //...and, as in `workspace_activate`, releases the old workspace's project pins if this
            // was the last client there.
            unpin_pushes = unpin_workspace_if_unused(&mut s, prior_id);
        }
    }
    let server_started_at = s.started_at_unix_ms;

    // Another client's open chooser should gain the new workspace.
    let mut pushes = unpin_pushes;
    pushes.extend(refresh_workspace_pickers(&mut s));
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }

    tracing::info!(%client_id, workspace = %name, "client created workspace");
    Ok(WorkspaceActivateResult {
        workspace: WorkspaceInfo {
            name,
            paths: Vec::new(),
            // A workspace created with no roots reaches no repo, so it can bind nothing.
            worktrees: Vec::new(),
            projects: Vec::new(),
        },
        last_view_id: None,
        opened: None,
        server_started_at,
    })
}

/// Whether this client is parked in a workspace. Its own lock, taken before the resolution block
/// below, because the answer decides whether to run [`workspace_activate`] — which takes the lock
/// itself and must not be called from inside one.
async fn client_has_workspace(state: &SharedState, client_id: ClientId) -> bool {
    state
        .lock()
        .await
        .clients
        .get(&client_id)
        .is_some_and(|c| c.active_workspace.is_some())
}

/// The configured workspace that owns `path`, when exactly one does. `None` for a path outside every
/// workspace (the temporary-context case) and for one that several claim with equal specificity —
/// a file handed over by the desktop has nobody to ask, and a temporary context is a better answer
/// than an error.
///
/// The rule itself is [`crate::config::infer_workspace_for_path_in`], shared with `aether-ae`'s
/// `resolve_workspace` — which the desktop open routes never reach, since Finder passes the document
/// by Apple event rather than in argv. Only the reading of an ambiguous answer differs between the
/// two; see [`crate::config::WorkspaceMatch`].
///
/// Reads the workspace TOMLs, so it runs outside the state lock (like `activate_context`'s cold
/// load) and via `workspaces_dir` rather than the profile default, so tests see their own tempdir.
async fn inferred_workspace(state: &SharedState, path: &std::path::Path) -> Option<String> {
    let dir = state.lock().await.workspaces_dir().ok()?;
    match crate::config::infer_workspace_for_path_in(&dir, path) {
        Ok(crate::config::WorkspaceMatch::One(name)) => Some(name),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(error = %e, "could not infer a workspace for an open-from-path");
            None
        }
    }
}

/// Open a path, resolving the workspace context (see [`WorkspaceOpenPath`]). Powers `ae PATH`, the
/// open-from-path overlay, and goto-definition into a file outside the active workspace. Internal
/// when the active workspace's roots contain the path; external when a workspace is active but
/// doesn't; a workspace inferred from the path — or, failing that, an ephemeral one — when none is.
///
/// A **directory** is a context rather than a thing to open (`ae ~/notes`): it roots the temporary
/// workspace at itself and lands on the landing buffer — a fresh transient scratch for a brand new
/// context — over which the client opens its explorer. Only a temporary context can take one; a
/// persisted workspace owns its roots, so a directory there is an error.
///
/// With no workspace active, a file the configured workspaces contain opens in the one that owns it
/// ([`crate::config::infer_workspace_for_path_in`], the rule `ae PATH` applies client-side). Failing
/// that we **join** a temporary context that already claims the path before minting a new one
/// ([`ServerState::ephemeral_workspace_for`]), so two clients opening the same external path share a
/// buffer instead of holding rival ones over the same document.
pub async fn workspace_open_path(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: WorkspaceOpenPathParams,
) -> Result<WorkspaceActivateResult, RpcError> {
    let client_id = ctx.client_id;
    let raw = crate::config::expand_home(std::path::Path::new(&params.path));
    // Require an absolute path (a leading `~/` counts — `expand_home` already made it absolute).
    // We deliberately don't resolve a relative path against the *server's* working directory: that
    // directory is meaningless to the user (the daemon's cwd, not theirs), so a relative open is
    // almost always a mistake. The CLI (`ae path`) pre-resolves its arg client-side before reaching
    // here, and goto-definition emits absolute paths, so only the open-from-path overlay can trip this.
    if !raw.is_absolute() {
        return Err(RpcError::invalid_path(format!(
            "open-from-path needs an absolute path (or one starting with ~/); got {:?}",
            params.path
        )));
    }
    // A path with no file behind it that some buffer is nonetheless *already bound to* — an unsaved
    // new file (`ae --web path/to/new-file`, written at the first save), which a second client is
    // now attaching to. Only ever *reaches* an existing buffer, so it creates nothing on its own and
    // needs no `create_if_missing`. `None` when the path resolves normally or nothing holds it.
    let mut attaching_to_unsaved = false;
    let canonical = match std::fs::canonicalize(&raw) {
        Ok(c) => c,
        // A not-yet-existing file (`ae path/to/new-file`): canonicalize the deepest existing
        // ancestor and keep the missing tail — the delegated `view/open` (which gets the same
        // `create_if_missing`) binds an empty buffer to it, written at the first save.
        Err(_) if params.create_if_missing => canonicalize_partial(&raw)
            .map_err(|e| RpcError::invalid_path(format!("resolving {}: {e}", raw.display())))?,
        Err(e) => {
            let partial = canonicalize_partial(&raw).ok();
            let held = match &partial {
                Some(p) => !state.lock().await.buffers_for_path(p).is_empty(),
                None => false,
            };
            match partial.filter(|_| held) {
                Some(p) => {
                    attaching_to_unsaved = true;
                    p
                }
                None => {
                    return Err(RpcError::invalid_path(format!(
                        "canonicalizing {}: {e}",
                        raw.display()
                    )))
                }
            }
        }
    };

    // A directory is a context, not a file to open — the one open with no buffer of its own. The
    // single probe here decides both the root adopted below and what gets opened at the end.
    let directory = canonical.is_dir();
    // The root a temporary context takes from this open: the directory itself, or the file's parent.
    let adopt_root = if directory {
        canonical.clone()
    } else {
        canonical.parent().unwrap_or(&canonical).to_path_buf()
    };

    // With **no workspace active at all**, a file inside a configured workspace opens *there*
    // rather than in a throwaway temporary context. Same rule `ae PATH` applies before it ever
    // connects, and for the opens that never pass through the CLI at all — macOS "Open With"
    // delivering `application:openURLs:`, a drop on the Dock icon — this is the only place it can
    // run, so a client that booted into the chooser used to land every such file in "(workspace 1)".
    //
    // Deliberately *only* when nothing is active. Once the client is somewhere, that is the answer:
    // an explicit open attaches to the workspace you are in (as a guest, if external) rather than
    // re-homing the file into some other workspace that happens to contain it.
    //
    // Directories keep their own rule (a temporary context rooted at the directory) — a persisted
    // workspace has no file here to open and refuses a directory outright, so inferring one would
    // turn `ae ~/some/repo/subdir`-shaped opens into an error.
    if !directory && !client_has_workspace(state, client_id).await {
        if let Some(name) = inferred_workspace(state, &canonical).await {
            tracing::info!(%client_id, workspace = %name, path = %canonical.display(), "open-from-path inferred a configured workspace");
            // `open_last: false` — this open has its own buffer to land on, resolved below.
            let activated = workspace_activate(
                state,
                ctx,
                WorkspaceActivateParams {
                    name: name.clone(),
                    // Unset, not empty: enter the context this workspace was last used in, the same
                    // "come back where I was" rule a launch with no `--worktree` gets.
                    worktrees: None,
                    open_last: false,
                },
            )
            .await;
            // Inference is an *improvement* on the temporary context, never a precondition for the
            // open. A workspace whose TOML lists a root that has since gone (so activation refuses
            // it) must not take the file down with it — fall through and open it the old way.
            if let Err(e) = activated {
                tracing::warn!(workspace = %name, error = %e.message, "inferred workspace would not activate; opening in a temporary context");
            }
        }
    }

    // Resolve the workspace this open lands in, activating an ephemeral one if the client has none.
    let (
        workspace_id,
        workspace_paths,
        server_started_at,
        created_ephemeral,
        superseded_pushes,
        watch_after,
    ) = {
        let mut s = state.lock().await;
        let active = s
            .clients
            .get(&client_id)
            .and_then(|c| c.active_workspace.clone());
        // Only a temporary context can adopt a directory. A persisted workspace owns its roots (and
        // has no file here to open), so a directory is refused before anything is mutated — which is
        // what the open-from-path overlay hits when a typed path turns out to be a directory.
        if directory
            && active
                .as_ref()
                .is_some_and(|id| !s.workspaces.get(id).is_some_and(|w| w.is_ephemeral()))
        {
            return Err(RpcError::invalid_path(format!(
                "{} is a directory, not a file",
                canonical.display()
            )));
        }
        let mut superseded_pushes = Vec::new();
        let (id, created) = match active.or_else(|| {
            // No workspace active: join the temporary context that already claims this path, if any
            // (`ephemeral_workspace_for`) — the same buffer rather than a rival one, which is how
            // the `--web` tether and the browser tab it opens end up on the same buffer.
            let joined = s.ephemeral_workspace_for(&canonical, directory);
            if let Some(id) = &joined {
                if let Some(session) = s.clients.get_mut(&client_id) {
                    session.active_workspace = Some(id.clone());
                }
                tracing::info!(%client_id, workspace = %id, "joined the temporary workspace holding this path");
            }
            joined
        }) {
            Some(id) => (id, false),
            None => {
                // A new temporary workspace replaces the idle ones before it, like a transient
                // buffer replaces the last preview — see `supersede_ephemeral_workspaces` for the
                // rule (dirty or still-in-use contexts survive).
                let (retired, closed, stopped) = s.supersede_ephemeral_workspaces();
                if !retired.is_empty() {
                    tracing::info!(
                        workspaces = ?retired,
                        buffers = closed.len(),
                        "superseded idle temporary workspaces"
                    );
                    superseded_pushes.extend(refresh_view_pickers(&mut s));
                }
                if !stopped.is_empty() {
                    superseded_pushes.extend(refresh_lsp_server_pickers(&mut s));
                }
                let id = s.register_ephemeral_workspace();
                if let Some(session) = s.clients.get_mut(&client_id) {
                    session.active_workspace = Some(id.clone());
                }
                tracing::info!(%client_id, workspace = %id, "activated ephemeral workspace for open-from-path");
                (id, true)
            }
        };
        // Root the temporary context at that directory so its pickers have something to work over
        // (`adopt_ephemeral_root`); a no-op for a persisted workspace, and for a path already under
        // a root of the temporary one.
        let adopted = s.adopt_ephemeral_root(&id, &adopt_root);
        if adopted {
            tracing::info!(workspace = %id, root = %adopt_root.display(), "temporary workspace adopted a root");
        }
        // A file open needs no separate watch registration — `view/open` watches the buffer's
        // parent directory, which is exactly the root just adopted. A directory open has no such
        // buffer, so register the new root here (after the lock, like `activate_context` does, since
        // registration walks the tree).
        let watch_after = (adopted && directory)
            .then(|| s.watcher.clone().map(|w| (w, vec![adopt_root.clone()])))
            .flatten();
        let paths = s
            .workspaces
            .get(&id)
            .map(|p| p.paths.iter().map(|p| p.display().to_string()).collect())
            .unwrap_or_default();
        (
            id,
            paths,
            s.started_at_unix_ms,
            created,
            superseded_pushes,
            watch_after,
        )
    };
    for (sender, notif) in superseded_pushes {
        let _ = sender.send(notif).await;
    }

    if let Some((w, roots)) = watch_after {
        tokio::task::spawn_blocking(move || crate::watcher::watch_workspace_paths(&w, &roots));
    }

    let opened = if directory {
        // Nothing to open: land where a client arriving in this context lands — its MRU buffer, or a
        // fresh transient scratch when there's nothing to return to (always so for a context this
        // open just minted). Exactly what `workspace/activate { open_last: true }` does on a first
        // visit to a configured workspace, so `ae DIR` feels the same either side of the boundary.
        let landing = {
            let s = state.lock().await;
            landing_view_id(&s, &workspace_id)
        };
        view_open(
            state,
            ctx,
            ViewOpenParams {
                view_id: landing,
                transient: landing.is_none().then_some(true),
                ..Default::default()
            },
        )
        .await?
    } else {
        view_open(
            state,
            ctx,
            ViewOpenParams {
                absolute_path: Some(canonical.display().to_string()),
                transient: params.transient,
                // The delegate canonicalizes again, and would refuse the unsaved-file path for the
                // same reason we didn't: pass the flag so it resolves, then its own
                // already-open-buffer reuse returns that buffer before anything is created.
                create_if_missing: params.create_if_missing || attaching_to_unsaved,
                jump_to: params.jump_to,
                ..Default::default()
            },
        )
        .await?
    };

    // A freshly-minted ephemeral workspace appears in any open switcher — and any it superseded
    // drops out of it, in the same rebuild.
    if created_ephemeral {
        let mut s = state.lock().await;
        let pushes = refresh_workspace_pickers(&mut s);
        drop(s);
        for (sender, notif) in pushes {
            let _ = sender.send(notif).await;
        }
    }

    let (projects, worktrees) = {
        let s = state.lock().await;
        (
            workspace_project_views_by_id(&s, &workspace_id),
            wire_bindings(&loaded_bindings(&s, &workspace_id)),
        )
    };
    Ok(WorkspaceActivateResult {
        workspace: WorkspaceInfo {
            name: workspace_id,
            paths: workspace_paths,
            worktrees,
            projects,
        },
        last_view_id: None,
        opened: Some(opened),
        server_started_at,
    })
}

/// Add a root path to an existing workspace. Canonicalizes, refuses duplicates, writes the TOML,
/// registers with the watcher, rebuilds the workspace index. Returns the updated workspace info.
pub async fn workspace_add_root(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: WorkspaceAddRootParams,
) -> Result<WorkspaceInfo, RpcError> {
    let canonical = crate::config::canonicalize_workspace_path(std::path::Path::new(&params.path))
        .map_err(|e| RpcError::invalid_path(format!("canonicalizing root: {e}")))?;
    // A root is a directory. This is the only RPC that writes one, so rejecting here (together with
    // the load-time filter in `config::load_workspace_in`) is what lets everything downstream treat
    // `WorkspaceEntry::paths` as directories without re-checking: the index walks them, the watcher
    // registers them, and `view/open` joins a relative path onto them.
    //
    // Safe after canonicalization, which already required the path to exist — so a `false` here
    // means "not a directory", never "not there yet".
    if !canonical.is_dir() {
        return Err(RpcError::invalid_path(format!(
            "workspace roots must be directories: {} is not one",
            canonical.display()
        )));
    }

    // A bound workspace's roots are a *materialisation* of its configured ones, so a root added to
    // it has to be materialised too — appended raw it would be the one root in the list that
    // ignores the binding. Resolved before the lock, since it reads the session file and walks for
    // a repo; an unbound workspace (the overwhelmingly common case) skips all of it.
    let (context, bound, bindings) = {
        let s = state.lock().await;
        let context = request_context(&s, ctx.client_id, &params.workspace)
            .ok_or_else(|| RpcError::unknown_workspace(&params.workspace))?;
        let bound = s
            .workspaces
            .get(&context)
            .is_some_and(|e| e.base_paths.is_some());
        let bindings = loaded_bindings(&s, &context);
        (context, bound, bindings)
    };
    let live = if bound {
        let (root, bindings) = (canonical.clone(), bindings);
        tokio::task::spawn_blocking(move || {
            crate::worktree::materialise_roots(&[root], &bindings).0
        })
        .await
        .map_err(|e| RpcError::internal(format!("resolving worktree bindings: {e}")))?
        .pop()
        .unwrap_or_else(|| canonical.clone())
    } else {
        canonical.clone()
    };

    let mut s = state.lock().await;
    let workspace = s
        .workspaces
        .get_mut(&context)
        .ok_or_else(|| RpcError::unknown_workspace(&params.workspace))?;
    // Checked against both halves: the client sends back a path it was shown, which is the live
    // one, while a hand-typed path is more likely the configured one.
    if workspace.paths.iter().any(|p| p == &live)
        || workspace.configured_paths().iter().any(|p| p == &canonical)
    {
        return Err(RpcError::invalid_params(format!(
            "{} is already a root of workspace {}",
            canonical.display(),
            params.workspace
        )));
    }
    // Applied to **every** context of this workspace, not just the caller's: a root is part of the
    // workspace's definition, and a sibling left without it would resolve paths against a shape the
    // workspace no longer has. Each materialises the new root against its *own* bindings, so a
    // bound context gets the worktree's copy of it and the base gets the configured one.
    let siblings = contexts_of(&s, &params.workspace);
    for id in siblings {
        let bindings = loaded_bindings(&s, &id);
        let live_here = if bindings.is_empty() {
            canonical.clone()
        } else {
            crate::worktree::materialise_roots(std::slice::from_ref(&canonical), &bindings)
                .0
                .pop()
                .unwrap_or_else(|| canonical.clone())
        };
        let Some(entry) = s.workspaces.get_mut(&id) else {
            continue;
        };
        // Appended to both halves in step, which is what keeps them positionally identical — the
        // property `ProjectRef::root_index` and `remap_path` both rest on.
        if let Some(base) = entry.base_paths.as_mut() {
            base.push(canonical.clone());
        }
        entry.paths.push(live_here);
        // Rebuild workspace_index with the new path list. The old Arc remains alive only for any
        // in-flight reader; subsequent picker opens see the fresh one.
        entry.workspace_index = Arc::new(crate::workspace_index::WorkspaceIndex::new(
            entry.paths.clone(),
        ));
    }
    let workspace = s
        .workspaces
        .get_mut(&context)
        .ok_or_else(|| RpcError::unknown_workspace(&params.workspace))?;
    // This rewrites the whole config file, so every field the workspace owns has to be carried
    // through — `projects` included, or editing a root would quietly delete them. Configured roots,
    // never the live ones (`WorkspaceEntry::configured_paths`).
    let updated = crate::config::WorkspaceConfig::from_parts(
        params.workspace.clone(),
        workspace.configured_paths(),
        &workspace.projects,
    );
    let entry_paths: Vec<String> = workspace
        .paths
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    let entry_worktrees = wire_bindings(&workspace.worktrees);
    let entry_projects = workspace_project_views(workspace);
    // The workspace is one thing however many clients are in it, so the others need the new shape
    // — their `workspace_paths` is what every path they render is resolved against.
    let pushes = workspace_changed_pushes(&s, &params.workspace, ctx.client_id);
    let watcher = s.watcher.clone();
    // Captured before the lock goes: the workspace store is a field on the state so a test
    // can point it at a tempdir instead of the developer's own configured workspaces.
    let workspaces_dir = s
        .workspaces_dir()
        .map_err(|e| RpcError::internal(format!("resolving workspaces dir: {e}")))?;
    drop(s);

    // TOML write + watcher registration happen outside the lock; registration also moves to a
    // blocking task so the response doesn't wait on the new root's (ignore-filtered) walk.
    crate::config::write_workspace_config_in(&workspaces_dir, &updated)
        .map_err(|e| RpcError::internal(format!("writing workspace config: {e}")))?;
    if let Some(w) = watcher {
        // The live path, not the configured one — the watcher registers directories that exist.
        tokio::task::spawn_blocking(move || crate::watcher::watch_workspace_paths(&w, &[live]));
    }
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    Ok(WorkspaceInfo {
        name: params.workspace,
        paths: entry_paths,
        worktrees: entry_worktrees,
        projects: entry_projects,
    })
}

/// Declare a project and pin its language server straight away.
///
/// Unlike activation — which skips entries that don't resolve so one stale project can't stop a
/// workspace loading — a *new* declaration fails loudly. The user is looking at the dialog, so
/// "that marker doesn't exist" is far more useful now than a silently ignored row later.
pub async fn workspace_add_project(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: WorkspaceAddProjectParams,
) -> Result<WorkspaceInfo, RpcError> {
    // A project is a directory relative to its root. An empty path means the root itself; store it
    // as `.` so the written config reads the same as a hand-edited one.
    let relative_path = match params.relative_path.trim().trim_end_matches('/') {
        "" => ".".to_string(),
        p => p.to_string(),
    };
    let project = crate::config::ProjectRef {
        root_index: params.path_index,
        relative_path: std::path::PathBuf::from(&relative_path),
        language: params.language,
    };

    let mut s = state.lock().await;
    // The caller's own context, not the workspace name: a bound context's entry is keyed by its
    // bindings, so the name alone is not in the map and would resolve to the base — editing and
    // reporting a shape the caller is not standing in.
    let context = request_context(&s, ctx.client_id, &params.workspace)
        .ok_or_else(|| RpcError::unknown_workspace(&params.workspace))?;
    let workspace = s
        .workspaces
        .get_mut(&context)
        .ok_or_else(|| RpcError::unknown_workspace(&params.workspace))?;
    if workspace
        .projects
        .iter()
        .any(|p| p.root_index == project.root_index && p.relative_path == project.relative_path)
    {
        return Err(RpcError::invalid_params(format!(
            "{} is already a project of workspace {}",
            relative_path, params.workspace
        )));
    }
    // Validate before committing anything — the message is the whole value of failing here.
    crate::config::resolve_project(&project, &workspace.paths).map_err(RpcError::invalid_params)?;

    workspace.projects.push(project);
    // Configured roots, not the live ones: a worktree-bound workspace's `paths` point into the
    // app-managed store, and writing those to the TOML would replace its definition with a checkout
    // git can delete (`WorkspaceEntry::configured_paths`).
    let updated = crate::config::WorkspaceConfig::from_parts(
        params.workspace.clone(),
        workspace.configured_paths(),
        &workspace.projects,
    );
    let entry_paths: Vec<String> = workspace
        .paths
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    let entry_worktrees = wire_bindings(&workspace.worktrees);
    // Start it now rather than at the next activation — the point of a pin is that indexing is
    // already under way by the time you want it.
    let launches = reconcile_workspace_pins(&mut s, &context);
    let mut pushes = refresh_lsp_server_pickers(&mut s);
    // Projects ride on `WorkspaceInfo` too, so the other clients' copy is stale without this.
    pushes.extend(workspace_changed_pushes(
        &s,
        &params.workspace,
        ctx.client_id,
    ));
    let entry_projects = workspace_project_views_by_id(&s, &context);
    // Captured before the lock goes: the workspace store is a field on the state so a test
    // can point it at a tempdir instead of the developer's own configured workspaces.
    let workspaces_dir = s
        .workspaces_dir()
        .map_err(|e| RpcError::internal(format!("resolving workspaces dir: {e}")))?;
    drop(s);

    crate::config::write_workspace_config_in(&workspaces_dir, &updated)
        .map_err(|e| RpcError::internal(format!("writing workspace config: {e}")))?;
    spawn_pinned_launches(state, launches);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }

    Ok(WorkspaceInfo {
        name: params.workspace,
        paths: entry_paths,
        worktrees: entry_worktrees,
        projects: entry_projects,
    })
}

/// The add-project row's live inference: the language a declaration of this directory would pin, or
/// `None` when the manifests don't single one out. Read-only, and never an error for a path that
/// doesn't resolve — the client asks as the user types, so half-typed paths are the normal input.
pub async fn workspace_infer_language(
    state: &SharedState,
    _ctx: &mut ConnectionCtx,
    params: WorkspaceInferLanguageParams,
) -> Result<WorkspaceInferLanguageResult, RpcError> {
    // Normalize exactly as `workspace_add_project` stores: trailing `/` trimmed, empty → `.` (the
    // root itself) — so the same-directory exclusion matches declarations however the path was typed.
    let relative_path = match params.relative_path.trim().trim_end_matches('/') {
        "" => ".".to_string(),
        p => p.to_string(),
    };
    let project = crate::config::ProjectRef {
        root_index: params.path_index,
        relative_path: std::path::PathBuf::from(&relative_path),
        language: None,
    };

    let s = state.lock().await;
    let workspace = s
        .workspaces
        .get(&params.workspace)
        .ok_or_else(|| RpcError::unknown_workspace(&params.workspace))?;
    let language =
        crate::config::infer_project_language(&project, &workspace.paths, &workspace.projects);
    Ok(WorkspaceInferLanguageResult { language })
}

/// Undeclare a project, unpinning its server (which then reaps unless buffers hold it up).
pub async fn workspace_remove_project(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: WorkspaceRemoveProjectParams,
) -> Result<WorkspaceInfo, RpcError> {
    // Matched as stored, not resolved: the entries most worth removing are the ones whose marker no
    // longer exists, so anything that touches the filesystem would fail on exactly those. Clients
    // send back the pair they were shown.
    let relative_path = std::path::PathBuf::from(&params.relative_path);

    let mut s = state.lock().await;
    // The caller's own context, not the workspace name: a bound context's entry is keyed by its
    // bindings, so the name alone is not in the map and would resolve to the base — editing and
    // reporting a shape the caller is not standing in.
    let context = request_context(&s, ctx.client_id, &params.workspace)
        .ok_or_else(|| RpcError::unknown_workspace(&params.workspace))?;
    let workspace = s
        .workspaces
        .get_mut(&context)
        .ok_or_else(|| RpcError::unknown_workspace(&params.workspace))?;
    let before = workspace.projects.len();
    workspace
        .projects
        .retain(|p| !(p.root_index == params.path_index && p.relative_path == relative_path));
    if workspace.projects.len() == before {
        return Err(RpcError::invalid_params(format!(
            "{} is not a project of workspace {}",
            params.relative_path, params.workspace
        )));
    }
    // Configured roots, not the live ones — see `workspace_add_project`.
    let updated = crate::config::WorkspaceConfig::from_parts(
        params.workspace.clone(),
        workspace.configured_paths(),
        &workspace.projects,
    );
    let entry_paths: Vec<String> = workspace
        .paths
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    let entry_worktrees = wire_bindings(&workspace.worktrees);
    // Reconcile drops the pin this project was holding; a server another project still wants, or
    // that has buffers open, stays up.
    let launches = reconcile_workspace_pins(&mut s, &context);
    let mut pushes = refresh_lsp_server_pickers(&mut s);
    // Projects ride on `WorkspaceInfo` too, so the other clients' copy is stale without this.
    pushes.extend(workspace_changed_pushes(
        &s,
        &params.workspace,
        ctx.client_id,
    ));
    let entry_projects = workspace_project_views_by_id(&s, &context);
    // Captured before the lock goes: the workspace store is a field on the state so a test
    // can point it at a tempdir instead of the developer's own configured workspaces.
    let workspaces_dir = s
        .workspaces_dir()
        .map_err(|e| RpcError::internal(format!("resolving workspaces dir: {e}")))?;
    drop(s);

    crate::config::write_workspace_config_in(&workspaces_dir, &updated)
        .map_err(|e| RpcError::internal(format!("writing workspace config: {e}")))?;
    spawn_pinned_launches(state, launches);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }

    Ok(WorkspaceInfo {
        name: params.workspace,
        paths: entry_paths,
        worktrees: entry_worktrees,
        projects: entry_projects,
    })
}

/// Spawn the handshakes for servers [`reconcile_workspace_pins`] created. Detached: a handshake plus
/// initial indexing takes seconds, and pinning exists precisely so that happens in the background.
pub fn spawn_pinned_launches(state: &SharedState, launches: Vec<PinnedLaunch>) {
    for (key, spec, generation) in launches {
        tokio::spawn(crate::lsp::manager::launch(
            state.clone(),
            key,
            spec,
            generation,
        ));
    }
}

/// Remove a root path from a workspace. Closes any file-backed buffers under this root that
/// aren't covered by another remaining root; refuses with `DIRTY_BUFFERS_PREVENT_REMOVE` if any
/// such buffer is dirty. Scratch buffers in the workspace are unaffected.
pub async fn workspace_remove_root(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: WorkspaceRemoveRootParams,
) -> Result<WorkspaceRemoveRootResult, RpcError> {
    let client_id = ctx.client_id;
    let canonical = crate::config::canonicalize_workspace_path(std::path::Path::new(&params.path))
        .map_err(|e| RpcError::invalid_path(format!("canonicalizing root: {e}")))?;

    let mut s = state.lock().await;
    // The caller's own context, not the workspace name: a bound context's entry is keyed by its
    // bindings, so the name alone is not in the map and would resolve to the base — editing and
    // reporting a shape the caller is not standing in.
    let context = request_context(&s, ctx.client_id, &params.workspace)
        .ok_or_else(|| RpcError::unknown_workspace(&params.workspace))?;
    let workspace = s
        .workspaces
        .get_mut(&context)
        .ok_or_else(|| RpcError::unknown_workspace(&params.workspace))?;
    if !workspace.paths.iter().any(|p| p == &canonical) {
        return Err(RpcError::invalid_params(format!(
            "{} is not a root of workspace {}",
            canonical.display(),
            params.workspace
        )));
    }
    let remaining_paths: Vec<std::path::PathBuf> = workspace
        .paths
        .iter()
        .filter(|p| **p != canonical)
        .cloned()
        .collect();
    let workspace_name = workspace.id.clone();

    // Find file-backed buffers under the removed root that aren't covered by any remaining
    // root. Scratch buffers (no path) are exempt; they stay alive.
    let under_removed = |buf: &Document| -> bool {
        let Some(p) = buf.canonical_path.as_deref() else {
            return false;
        };
        p == canonical || p.starts_with(&canonical)
    };
    let still_covered = |buf: &Document| -> bool {
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
            let Some(doc) = s.documents.get(&buf.document) else {
                return false;
            };
            s.buffer_workspaces.get(id).map(|s| s.as_str()) == Some(&workspace_name)
                && under_removed(doc)
                && !still_covered(doc)
        })
        .map(|(id, _)| *id)
        .collect();
    let dirty: Vec<BufferId> = affected
        .iter()
        .filter(|id| s.try_doc_of(**id).map(|d| d.dirty).unwrap_or(false))
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
    let other_clients = clients_affected_by_close(&s, &affected, client_id);
    // Close the affected buffers (clean ones). Same teardown as view/close.
    for &id in &affected {
        s.close_buffer(id);
    }

    // Persist the updated path list. Re-grab the workspace mutably for the write.
    let workspace = s
        .workspaces
        .get_mut(&context)
        .expect("workspace still loaded — we held it above");
    // Drop the root, then repair the projects that referred to it *by position*. The config file
    // nests projects under their root precisely so this can't go wrong on disk; in memory they're
    // flat, so this is the one place the index has to be maintained by hand — and getting it wrong
    // silently re-points a project at a different root.
    //
    // Found by position in *either* half: the client sends back the path it was shown, which for a
    // bound workspace is the live one, while a hand-typed path is more likely the configured one.
    // Whichever names it, the same index is dropped from both — they are positionally identical, so
    // removing by value from one alone would slide them out of step.
    let removed_index = workspace
        .paths
        .iter()
        .position(|p| *p == canonical)
        .or_else(|| {
            workspace
                .configured_paths()
                .iter()
                .position(|p| *p == canonical)
        });
    if let Some(i) = removed_index {
        workspace.paths.remove(i);
        if let Some(base) = workspace.base_paths.as_mut() {
            base.remove(i);
        }
        crate::config::drop_root_from_projects(&mut workspace.projects, i as u32);
    }
    workspace.workspace_index = Arc::new(crate::workspace_index::WorkspaceIndex::new(
        workspace.paths.clone(),
    ));
    // Full-file rewrite — carry `projects` through, and write the *configured* roots, never the
    // live ones (see `workspace_add_root`).
    let updated = crate::config::WorkspaceConfig::from_parts(
        params.workspace.clone(),
        workspace.configured_paths(),
        &workspace.projects,
    );
    let entry_paths: Vec<String> = workspace
        .paths
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    let entry_worktrees = wire_bindings(&workspace.worktrees);
    let entry_projects = workspace_project_views(workspace);

    // Next buffer for the requesting client: top of workspace MRU, else any remaining buffer in
    // the workspace. Mirrors view/close.
    let next_view_id = next_view_for_client(&s, client_id);
    let watcher = s.watcher.clone();
    // Shape first, then what closed — same order as a worktree rebind, and for the same reason:
    // the close makes a client open its successor, and that open resolves the path against the
    // roots it holds.
    let mut pushes = workspace_changed_pushes(&s, &params.workspace, client_id);
    pushes.extend(refresh_view_pickers(&mut s));
    pushes.extend(buffer_closed_pushes(&s, &other_clients));
    // Captured before the lock goes: the workspace store is a field on the state so a test
    // can point it at a tempdir instead of the developer's own configured workspaces.
    let workspaces_dir = s
        .workspaces_dir()
        .map_err(|e| RpcError::internal(format!("resolving workspaces dir: {e}")))?;
    drop(s);

    crate::config::write_workspace_config_in(&workspaces_dir, &updated)
        .map_err(|e| RpcError::internal(format!("writing workspace config: {e}")))?;
    if let Some(w) = watcher {
        crate::watcher::unwatch_workspace_paths(&w, &[canonical]);
        // The unwatch drops every registered directory under the removed root — including any a
        // *different* loaded workspace with an overlapping root still needs. Self-heal by
        // re-walking all loaded roots (idempotent, ignore-filtered, debounced).
        crate::watcher::schedule_rescan(state.clone(), w);
    }
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    tracing::info!(
        workspace = %params.workspace,
        closed = affected.len(),
        "root removed"
    );
    Ok(WorkspaceRemoveRootResult {
        workspace: WorkspaceInfo {
            name: params.workspace,
            paths: entry_paths,
            worktrees: entry_worktrees,
            projects: entry_projects,
        },
        closed_buffer_ids: affected,
        next_view_id,
    })
}

/// Rename a workspace: move its on-disk config, then re-key every in-memory reference to the old
/// name (the workspace map, buffer→workspace associations, and clients' active-workspace pointers).
/// Open buffers keep their ids and paths and nothing is closed, so this is safe regardless of
/// dirty state. Refuses an empty / separator-bearing name or a collision with an existing
/// workspace; a no-op when the name is unchanged.
pub async fn workspace_rename(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: WorkspaceRenameParams,
) -> Result<WorkspaceInfo, RpcError> {
    let new_name = validate_workspace_name(&params.new_name)?;
    let old_name = params.workspace;

    // Confirm the workspace is loaded *before* touching disk, so a failure here leaves nothing
    // half-applied. Workspaces are never removed from the map at runtime, so this stays true for
    // the re-key below.
    {
        let s = state.lock().await;
        let entry = s
            .workspaces
            .get(&old_name)
            .ok_or_else(|| RpcError::unknown_workspace(&old_name))?;
        if new_name == old_name {
            // No-op rename — return current info without touching disk or state.
            return Ok(WorkspaceInfo {
                name: old_name,
                paths: entry
                    .paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect(),
                worktrees: wire_bindings(&entry.worktrees),
                projects: workspace_project_views(entry),
            });
        }
    }

    // Refuse clobbering another workspace's config; `fs::rename` would otherwise overwrite it.
    let workspaces_dir = state
        .lock()
        .await
        .workspaces_dir()
        .map_err(|e| RpcError::internal(format!("resolving workspaces dir: {e}")))?;
    let exists = crate::config::workspace_config_exists_in(&workspaces_dir, &new_name);
    if exists {
        return Err(RpcError::invalid_params(format!(
            "workspace {new_name} already exists"
        )));
    }

    // Disk first, outside the lock (file I/O). If this fails, in-memory state is untouched.
    crate::config::rename_workspace_config_in(&workspaces_dir, &old_name, &new_name)
        .map_err(|e| RpcError::internal(format!("renaming workspace config: {e}")))?;

    // Carry the persisted session across to the new name (recency stamp + restored buffers). Held
    // under the state lock like every other session-file write, so it can't race a concurrent
    // persist. Best-effort; the workspace still works without it, so a failure only logs.
    {
        let mut s = state.lock().await;
        if let Some(path) = s.sessions_path.clone() {
            if let Err(e) = crate::config::rename_workspace_session_at(&path, &old_name, &new_name)
            {
                tracing::warn!(old = %old_name, new = %new_name, error = %e, "failed to rename workspace session");
            }
        }
        // The input-history lists follow the name too — they're keyed by workspace, and a rename
        // shouldn't read as "history lost". In-memory + dirty flag; the periodic flush writes it,
        // like every other `history.json` mutation.
        if let Some(lists) = s.history.workspaces.remove(&old_name) {
            s.history.workspaces.insert(new_name.clone(), lists);
            s.history_dirty = true;
        }
    }

    // Re-key every in-memory reference from the old name to the new one. Workspaces are never
    // removed from the map at runtime, so the entry we confirmed above is still present.
    let mut s = state.lock().await;
    let entry_paths = s
        .rename_workspace(&old_name, &new_name)
        .ok_or_else(|| RpcError::internal("workspace vanished during rename"))?;

    // The re-key above already moved every other connected client on this workspace to the new name
    // server-side; push `workspace/renamed` so each can update its *local* name (display + reconnect
    // baseline). The initiating client learns the new name from this RPC's result instead.
    let mut pushes: PendingPushes = s
        .clients
        .iter()
        .filter(|(id, sess)| {
            **id != ctx.client_id && sess.active_workspace.as_deref() == Some(new_name.as_str())
        })
        .map(|(_, sess)| {
            (
                sess.outbound.clone(),
                Notification {
                    jsonrpc: JsonRpc,
                    method: WorkspaceRenamed::NAME.into(),
                    params: serde_json::to_value(WorkspaceRenamedParams {
                        old_name: old_name.clone(),
                        new_name: new_name.clone(),
                    })
                    .unwrap_or(serde_json::Value::Null),
                },
            )
        })
        .collect();
    //...and any open chooser elsewhere should show the new name in its list.
    pushes.extend(refresh_workspace_pickers(&mut s));
    // Re-keyed above, so the projects come from the *new* name.
    let entry_projects = workspace_project_views_by_id(&s, &new_name);
    let entry_worktrees = s
        .workspaces
        .get(&new_name)
        .map(|e| wire_bindings(&e.worktrees))
        .unwrap_or_default();
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }

    tracing::info!(old = %old_name, new = %new_name, "workspace renamed");
    Ok(WorkspaceInfo {
        name: new_name,
        paths: entry_paths,
        worktrees: entry_worktrees,
        projects: entry_projects,
    })
}

/// Delete a workspace: drop its in-memory state (closing its buffers) and remove its on-disk config.
/// Forgets the workspace *definition* — source files under its roots are untouched. Refuses if the
/// workspace is active for any client (the caller must switch away first), or if any of its buffers
/// is dirty.
pub async fn workspace_delete(
    state: &SharedState,
    _ctx: &mut ConnectionCtx,
    params: WorkspaceDeleteParams,
) -> Result<(), RpcError> {
    let name = params.name;

    let mut s = state.lock().await;

    // Refuse to delete a workspace anyone is currently in — that's the rug-pull we promised to
    // prevent. The switcher already greys out the caller's own active workspace; this also covers
    // other connected clients.
    if s.workspace_active_anywhere(&name) {
        return Err(RpcError::new(
            ErrorCode::ACTIVE_WORKSPACE_PREVENTS_DELETE,
            format!("workspace {name} is active — switch to another workspace before deleting it"),
        ));
    }

    // Refuse if any buffer in the workspace has unsaved changes (mirrors `workspace/remove_root`).
    let dirty: Vec<BufferId> = s
        .buffers_in_workspace(&name)
        .into_iter()
        .filter(|id| s.try_doc_of(*id).map(|d| d.dirty).unwrap_or(false))
        .collect();
    if !dirty.is_empty() {
        let mut err = RpcError::new(
            ErrorCode::DIRTY_BUFFERS_PREVENT_DELETE,
            format!(
                "{} buffer(s) in workspace {name} have unsaved changes",
                dirty.len()
            ),
        );
        err.data = Some(serde_json::json!({ "dirty_buffer_ids": dirty }));
        return Err(err);
    }

    let closed = s.delete_workspace(&name);
    // Intentionally leave the (now-orphaned) workspace roots in the watcher: dropping a watch is
    // best-effort and a sibling workspace may share the same root. Stale watches are harmless — the
    // watcher drops events that don't map to a loaded workspace.
    drop(s);

    let workspaces_dir = state
        .lock()
        .await
        .workspaces_dir()
        .map_err(|e| RpcError::internal(format!("resolving workspaces dir: {e}")))?;
    crate::config::delete_workspace_config_in(&workspaces_dir, &name)
        .map_err(|e| RpcError::internal(format!("deleting workspace config: {e}")))?;

    // Drop its persisted session too, so a deleted workspace doesn't leave an orphan behind. Held
    // under the state lock like every other session-file write, so it can't race a concurrent
    // persist. Best-effort; a stale entry is harmless (it'd just never be listed), so we only log.
    {
        let mut s = state.lock().await;
        if let Some(path) = s.sessions_path.clone() {
            if let Err(e) = crate::config::remove_workspace_session_at(&path, &name) {
                tracing::warn!(workspace = %name, error = %e, "failed to remove workspace session");
            }
        }
        // Same for its input history — a deleted workspace leaves no orphan lists behind.
        if s.history.workspaces.remove(&name).is_some() {
            s.history_dirty = true;
        }
    }

    // Re-take the lock to refresh any open chooser — only now is the workspace gone from disk (the
    // candidate list is a disk read), so the dropped workspace disappears from the list.
    let pushes = {
        let mut s = state.lock().await;
        refresh_workspace_pickers(&mut s)
    };
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }

    tracing::info!(workspace = %name, closed = closed.len(), "workspace deleted");
    Ok(())
}

/// Delete a file or directory by moving it to the OS trash. Validates the path is inside the
/// active workspace (and isn't a root itself), refuses if it — or, for a directory, anything under
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
        let workspace = s.active_workspace_or_err(client_id)?;
        if !workspace.contains(&canonical) {
            return Err(RpcError::invalid_path(format!(
                "{} is outside the workspace's access boundary",
                canonical.display()
            )));
        }
        if workspace.paths.iter().any(|p| p == &canonical) {
            return Err(RpcError::invalid_params(format!(
                "{} is a workspace root — remove it from workspace settings instead",
                canonical.display()
            )));
        }
        let workspace_name = workspace.id.clone();
        let dirty: Vec<BufferId> = s
            .buffers_under_path(&workspace_name, &canonical)
            .into_iter()
            .filter(|id| s.try_doc_of(*id).map(|d| d.dirty).unwrap_or(false))
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
    let Some(workspace_name) = s
        .clients
        .get(&client_id)
        .and_then(|c| c.active_workspace.clone())
    else {
        // Client deactivated mid-call — the trash already happened; nothing left to tear down.
        return Ok(PathDeleteResult {
            closed_buffer_ids: Vec::new(),
            next_view_id: None,
        });
    };
    let closed = s.buffers_under_path(&workspace_name, &canonical);
    // Other clients viewing any of these buffers must be told to switch — capture before teardown.
    let other_clients = clients_affected_by_close(&s, &closed, client_id);
    for &id in &closed {
        s.close_buffer(id);
    }
    // Drop the Files-picker cache so a re-view re-walks without the deleted path. The watcher will
    // also notice the removal, but this keeps the client's immediate refresh consistent.
    if let Some(p) = s.workspaces.get(&workspace_name) {
        p.workspace_index.invalidate();
    }
    let next_view_id = next_view_for_client(&s, client_id);
    let mut pushes = refresh_view_pickers(&mut s);
    pushes.extend(buffer_closed_pushes(&s, &other_clients));
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    tracing::info!(path = %canonical.display(), closed = closed.len(), "path trashed");
    Ok(PathDeleteResult {
        closed_buffer_ids: closed,
        next_view_id,
    })
}

/// Enumerate the workspaces configured on disk under `$XDG_CONFIG_HOME/aether/workspaces/`. The
/// caller uses this to populate the workspace picker. Doesn't indicate which workspace (if any) the
/// caller has active — the client tracks that locally.
pub async fn workspace_list(
    state: &SharedState,
    _ctx: &mut ConnectionCtx,
    _params: WorkspaceListParams,
) -> Result<WorkspaceListResult, RpcError> {
    let names = {
        let s = state.lock().await;
        s.workspaces_dir()
            .and_then(|d| crate::config::list_workspace_names_in(&d))
            .map_err(|e| RpcError::internal(format!("listing workspaces: {e}")))?
    };
    Ok(WorkspaceListResult {
        workspaces: names
            .into_iter()
            .map(|name| WorkspaceSummary { name })
            .collect(),
    })
}

#[cfg(test)]
mod restore_tests {
    use super::*;
    use crate::config::SessionView;
    use crate::state::DormantSource;

    /// `restore_dormant_sources` decides what comes back as dormant buffers after a restart. Files
    /// need to still exist (or have a backup); scratches need a backup — from the session entry, or
    /// recovered by scanning the backups dir for numbers the session never recorded.
    #[test]
    fn restore_dormant_sources_files_and_scratches() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let backups = root.join("backups");

        // An existing file on disk; a deleted file that still has a backup; a deleted file with no
        // backup (dropped).
        let present = root.join("present.rs");
        std::fs::write(&present, "x\n").unwrap();
        let deleted_with_backup = root.join("gone_kept.rs");
        let deleted_no_backup = root.join("gone_dropped.rs");
        crate::backup::write(
            &crate::backup::file_backup_path(&backups, &deleted_with_backup),
            "unsaved\n",
        )
        .unwrap();

        // Scratch 1 recorded in the session with a backup; scratch 2 recorded but with NO backup
        // (dropped); scratch 5 NOT in the session but present on disk (recovered by the scan).
        crate::backup::write(&crate::backup::scratch_backup_path(&backups, "p", 1), "s1").unwrap();
        crate::backup::write(&crate::backup::scratch_backup_path(&backups, "p", 5), "s5").unwrap();

        let entries = vec![
            SessionView::Editor {
                path: present.clone(),
            },
            SessionView::Editor {
                path: deleted_with_backup.clone(),
            },
            SessionView::Editor {
                path: deleted_no_backup.clone(),
            },
            SessionView::Scratch { number: 1 },
            SessionView::Scratch { number: 2 },
        ];

        let sources: Vec<DormantSource> = restore_dormant_sources(&entries, "p", Some(&backups))
            .into_iter()
            .map(|(source, _)| source)
            .collect();
        assert_eq!(
            sources,
            vec![
                DormantSource::File(present),
                DormantSource::File(deleted_with_backup),
                // deleted_no_backup dropped; scratch 2 (no backup) dropped.
                DormantSource::Scratch { number: 1 },
                // scratch 5 recovered from the backup dir scan, after the session entries.
                DormantSource::Scratch { number: 5 },
            ]
        );
    }

    /// A kept revision comes back unconditionally: there is nothing on disk to probe for, because
    /// its content is regenerated from the repo on first view. A commit that has been rebased away
    /// since reports itself then — when `git/show` can say what went wrong — rather than being
    /// silently dropped here, where the only honest message would be "something didn't restore".
    #[test]
    fn restore_dormant_sources_keeps_revisions() {
        let entries = vec![
            SessionView::Virtual {
                key: "/repo@abc1234".into(),
            },
            SessionView::Virtual {
                key: "/repo@abc1234:src/a.rs".into(),
            },
        ];
        // No backups dir at all: a revision doesn't need one, unlike a scratch.
        let sources: Vec<DormantSource> = restore_dormant_sources(&entries, "p", None)
            .into_iter()
            .map(|(source, _)| source)
            .collect();
        assert_eq!(
            sources,
            vec![
                DormantSource::Virtual {
                    key: "/repo@abc1234".into()
                },
                DormantSource::Virtual {
                    key: "/repo@abc1234:src/a.rs".into()
                },
            ]
        );
    }

    /// With backups disabled, a file is restorable only if it still exists, and no scratch is ever
    /// restorable (no backup to read).
    #[test]
    fn restore_dormant_sources_without_backups() {
        let dir = tempfile::tempdir().unwrap();
        let present = dir.path().join("a.rs");
        std::fs::write(&present, "x\n").unwrap();
        let entries = vec![
            SessionView::Editor {
                path: present.clone(),
            },
            SessionView::Editor {
                path: dir.path().join("missing.rs"),
            },
            SessionView::Scratch { number: 1 },
        ];
        assert_eq!(
            restore_dormant_sources(&entries, "p", None),
            vec![(
                DormantSource::File(present),
                Some(aether_protocol::ui::ViewKind::Editor)
            )]
        );
    }
}

#[cfg(test)]
mod workspace_name_tests {
    use super::validate_workspace_name;

    #[test]
    fn trims_surrounding_whitespace_and_accepts() {
        assert_eq!(validate_workspace_name("  my-proj  ").unwrap(), "my-proj");
        assert_eq!(validate_workspace_name("aether").unwrap(), "aether");
    }

    #[test]
    fn rejects_empty_blank_and_path_separators() {
        for bad in ["", "   ", "a/b", "a\\b", ".", ".."] {
            assert!(
                validate_workspace_name(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn rejects_the_reserved_ephemeral_name() {
        // A workspace called `ephemeral` would give its worktree variants ids of the form
        // `ephemeral/<x>`, which `is_ephemeral_workspace_id` reads as a throwaway context —
        // silently costing a persisted workspace its session. Refused where the name is chosen.
        assert!(validate_workspace_name("ephemeral").is_err());
        assert!(validate_workspace_name("  ephemeral  ").is_err());
        // Only the exact word: nothing else in that neighbourhood is reserved.
        assert!(validate_workspace_name("ephemerals").is_ok());
        assert!(validate_workspace_name("my-ephemeral").is_ok());
    }
}
