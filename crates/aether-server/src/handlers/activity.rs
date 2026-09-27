//! `activity/*` — a workspace's work in progress, as one list: shells running a command, agents
//! working through a turn, and the git operations the user started.
//!
//! **Derived, never stored.** The list is read off the things that are running — a transcript's
//! active run, a conversation's turn, [`ServerState::git_operations`] — every time it is needed,
//! so it cannot disagree with them. What each transition does is [`push_activity`]: tell every
//! client its workspace's list again. The list is a handful of entries; sending it whole is cheaper
//! than getting a delta wrong.

use super::*;
use aether_protocol::activity::{
    Activity, ActivityCancelParams, ActivityCancelResult, ActivityChanged, ActivityChangedParams,
    ActivityId,
};
use aether_protocol::shell::ShellCancelParams;

/// The work in progress `client_id` is told about: the shells and conversations of its active
/// workspace, then the git operations in repos it can see. Views in the order they were made, so
/// a row does not move when another starts or finishes.
pub(crate) fn activity_for(s: &ServerState, client_id: ClientId) -> Vec<Activity> {
    let Some(workspace) = s
        .clients
        .get(&client_id)
        .and_then(|c| c.active_workspace.clone())
    else {
        return Vec::new();
    };
    let mut views: Vec<(&ViewId, &crate::state::View)> = s
        .views
        .iter()
        .filter(|(_, v)| s.buffer_workspaces.get(&v.presenting) == Some(&workspace))
        .collect();
    views.sort_by_key(|(id, _)| **id);
    let mut out: Vec<Activity> = views
        .into_iter()
        .filter_map(|(view_id, view)| {
            let doc = s.try_doc_of(view.presenting)?;
            if let Some(t) = doc.transcript() {
                let run = t.active()?;
                return Some(Activity {
                    id: ActivityId::Shell { view_id: *view_id },
                    owner: t.title.clone(),
                    label: run.command.clone(),
                });
            }
            let c = doc.conversation()?;
            let turn = c.turn.as_ref()?;
            Some(Activity {
                id: ActivityId::Agent { view_id: *view_id },
                owner: c.title.clone(),
                label: turn
                    .activity
                    .clone()
                    .unwrap_or_else(|| "thinking".to_string()),
            })
        })
        .collect();
    let mut git: Vec<(&std::path::PathBuf, &crate::state::GitOperationEntry)> =
        s.git_operations.iter().collect();
    git.sort_by(|a, b| a.0.cmp(b.0));
    for (workdir, op) in git {
        let repo_id = workdir.to_string_lossy().into_owned();
        // A client only hears about repos it could act on — the rule `git/cancel` resolves by.
        if resolve_repo(s, client_id, &repo_id).is_err() {
            continue;
        }
        out.push(Activity {
            id: ActivityId::Git { repo_id },
            owner: workdir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| workdir.to_string_lossy().into_owned()),
            label: op.kind.label().to_string(),
        });
    }
    out
}

/// Tell every client its workspace's work in progress, and re-push every open activity picker.
/// Called by each start, finish and relabel — the one funnel, so the status bar's count, the picker
/// and the list cannot be told different things.
pub(crate) async fn push_activity(state: &SharedState) {
    let pushes: PendingPushes = {
        let mut s = state.lock().await;
        let mut pushes = refresh_activity_pickers(&mut s);
        pushes.extend(
            s.clients
                .keys()
                .filter_map(|client_id| activity_notification(&s, *client_id)),
        );
        pushes
    };
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
}

/// Tell one client its workspace's work in progress — what a client that has just changed
/// workspace needs, since everything it was told before was about the one it left.
pub(crate) async fn push_activity_to(state: &SharedState, client_id: ClientId) {
    let push = {
        let s = state.lock().await;
        activity_notification(&s, client_id)
    };
    if let Some((sender, notif)) = push {
        let _ = sender.send(notif).await;
    }
}

/// The clients whose active workspace is the one `view_id`'s view belongs to — who a shell's or a
/// conversation's own notices go to. None when the view has gone.
pub(crate) fn clients_of_view(
    s: &ServerState,
    view_id: ViewId,
) -> Vec<tokio::sync::mpsc::Sender<Notification>> {
    let Some(workspace) = s
        .try_presenting_buffer(view_id)
        .and_then(|b| s.buffer_workspaces.get(&b))
    else {
        return Vec::new();
    };
    s.clients
        .values()
        .filter(|c| c.active_workspace.as_ref() == Some(workspace))
        .map(|c| c.outbound.clone())
        .collect()
}

fn activity_notification(
    s: &ServerState,
    client_id: ClientId,
) -> Option<(tokio::sync::mpsc::Sender<Notification>, Notification)> {
    let sender = s.clients.get(&client_id)?.outbound.clone();
    let params = ActivityChangedParams {
        items: activity_for(s, client_id),
    };
    Some((
        sender,
        Notification {
            jsonrpc: JsonRpc,
            method: ActivityChanged::NAME.into(),
            params: serde_json::to_value(&params).unwrap_or(serde_json::Value::Null),
        },
    ))
}

/// Stop one piece of work — through the cancel its kind already has, so a shell's command goes with
/// its whole process group, an agent's turn is cancelled as the agent is told to, and a git
/// operation is killed exactly as `git/cancel` kills it.
pub async fn activity_cancel(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: ActivityCancelParams,
) -> Result<ActivityCancelResult, RpcError> {
    let cancelled = match params.id {
        ActivityId::Shell { view_id } => {
            shell_cancel(state, ctx, ShellCancelParams { view_id })
                .await?
                .cancelled
        }
        ActivityId::Agent { view_id } => {
            agent_cancel(
                state,
                ctx,
                aether_protocol::agent::AgentCancelParams { view_id },
            )
            .await?
            .cancelled
        }
        ActivityId::Git { repo_id } => {
            git_cancel(state, ctx, GitCancelParams { repo_id })
                .await?
                .cancelled
        }
    };
    Ok(ActivityCancelResult { cancelled })
}
