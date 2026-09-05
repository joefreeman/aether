//! Application-scope handlers: `app/info` (the build/instance snapshot behind `Space ?` and
//! `GET /status`), `settings/*`, and `hints/*`.

use super::*;

/// Snapshot the running application for the info dialog (`Space ?`): build identity, live instance,
/// on-disk state locations. App-global — no active workspace required, which matters because one
/// reason to open the dialog is that the client is in a state where nothing else works.
///
/// The same snapshot backs `GET /status` (see [`crate::status`]), so the dialog and
/// `ae server status` can never disagree about what the server is.
pub async fn app_info(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    _params: AppInfoParams,
) -> Result<AppInfo, RpcError> {
    // Probe git *between* the two locks, never under one: it spawns a child process, and the
    // first call for a root also launches a login shell to resolve the environment. Holding the
    // state lock across that would stall every other client. Probed in the active workspace's
    // first root so the answer reflects the `PATH` git would really run under there.
    let probe_dir = {
        let s = state.lock().await;
        s.active_workspace(ctx.client_id)
            .and_then(|w| w.paths.first().cloned())
    };
    let git_version = crate::git_cli::version_in(probe_dir.as_deref()).await;

    let s = state.lock().await;
    Ok(crate::status::app_info(&s, git_version))
}

/// Read the global application settings (`$XDG_CONFIG_HOME/aether/settings.toml`). Returns defaults
/// when no settings file exists yet. App-wide, so it ignores the caller's active workspace.
pub async fn settings_get(
    state: &SharedState,
    _ctx: &mut ConnectionCtx,
    _params: SettingsGetParams,
) -> Result<AppSettings, RpcError> {
    let path = state
        .lock()
        .await
        .app_settings_path()
        .map_err(|e| RpcError::internal(format!("resolving app settings path: {e}")))?;
    crate::config::load_app_settings_at(&path)
        .map_err(|e| RpcError::internal(format!("loading app settings: {e}")))
}

/// Replace the global application settings and persist them. Echoes the stored settings back, so
/// the caller reconciles against exactly what landed on disk, and pushes `settings/changed` to every
/// *other* connected client (settings are app-wide, so this ignores active workspaces) so the change
/// applies live everywhere rather than only at the next reconnect.
pub async fn settings_set(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: AppSettings,
) -> Result<AppSettings, RpcError> {
    let path = state
        .lock()
        .await
        .app_settings_path()
        .map_err(|e| RpcError::internal(format!("resolving app settings path: {e}")))?;
    crate::config::write_app_settings_at(&path, &params)
        .map_err(|e| RpcError::internal(format!("writing app settings: {e}")))?;

    let changed = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
    let pushes: PendingPushes = {
        let mut s = state.lock().await;
        s.app_settings = params.clone();
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

// ---- hints/* ------------------------------------------------------------------------------------

/// Apply one hint event. App-global like settings — no active workspace required. The server stamps
/// the wall clock (day attribution and fatigue decay are its call, so two windows can't disagree)
/// and derives retirement; the write to `hints.json` rides the periodic dirty-flag flush
/// ([`flush_hints`]), not this request.
pub async fn hints_record(
    state: &SharedState,
    _ctx: &mut ConnectionCtx,
    params: HintsRecordParams,
) -> Result<HintsRecordResult, RpcError> {
    let now_ms = crate::config::now_unix_ms();
    let mut s = state.lock().await;
    let (changed, retired) = s.hints.apply(&params.hint_id, params.event, now_ms);
    if changed {
        s.hints_dirty = true;
    }
    Ok(HintsRecordResult { retired })
}

/// The full hint learning-state snapshot, from memory. Fetched once per connection alongside
/// `settings/get`.
pub async fn hints_state(
    state: &SharedState,
    _ctx: &mut ConnectionCtx,
    _params: HintsStateParams,
) -> Result<HintsStateResult, RpcError> {
    let s = state.lock().await;
    Ok(s.hints.snapshot())
}

/// Write the hint learning state to disk when it changed since the last flush. The periodic loop
/// in `server` calls this every second (that's the debounce — hint events between ticks coalesce
/// into one write) and once more on graceful shutdown. Best-effort like [`flush_backups`]: a no-op
/// when `hints_path` is unset; logs rather than fails on I/O error. The write happens off the lock
/// — the state is tiny to clone and `hints.json` has a single writer (this function).
pub(crate) async fn flush_hints(state: &SharedState) {
    let (path, snapshot) = {
        let mut s = state.lock().await;
        let Some(path) = s.hints_path.clone() else {
            return;
        };
        if !s.hints_dirty {
            return;
        }
        s.hints_dirty = false;
        (path, s.hints.clone())
    };
    if let Err(e) = crate::config::write_hints_at(&path, &snapshot) {
        tracing::warn!(error = %e, "failed to write hint state");
    }
}
