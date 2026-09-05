//! `git/*` — repository operations: show/diff, staging, commit, branches, worktrees, stash, remotes, conflicts.

use super::*;

/// Resolve a client-supplied [`RepoId`] against the set its active workspace can actually reach,
/// erroring on anything else.
///
/// Ids are paths, so a client *could* synthesize one; validating every incoming id against the
/// reachable set is what makes that harmless. Note this checks reachability only — a mutating RPC
/// must additionally require a non-empty [`GitRepoInfo::roots`], since a repo reached solely
/// through an open buffer is readable but is not something the user has opened for editing.
pub fn resolve_repo(
    s: &ServerState,
    client_id: ClientId,
    repo_id: &str,
) -> Result<GitRepoInfo, RpcError> {
    reachable_repos(s, client_id)?
        .into_iter()
        .find(|r| r.repo_id == repo_id)
        .ok_or_else(|| RpcError::repo_not_found(repo_id))
}

/// The distinct repos the active workspace can reach: one per canonicalized working directory,
/// roots first (in workspace order), then repos reached only through an open buffer.
///
/// Resolved on demand rather than cached. Discovery runs once per *root* — a handful of walks —
/// and open buffers cost nothing at all, since a buffer's repo is already sitting in its cached
/// Git baseline. There's nothing here worth invalidating yet; when the CLI runner needs somewhere
/// to hang a per-repo operation lock, that's the point to introduce a registry.
///
/// **Not a candidate set** — no RPC picks a repo out of this. It exists to *validate*, saying
/// whether a repo a buffer or a client named is one this workspace can act on, and to fill
/// [`GitRepoInfo::roots`], which is what separates a readable repo from a writable one.
pub fn reachable_repos(s: &ServerState, client_id: ClientId) -> Result<Vec<GitRepoInfo>, RpcError> {
    let workspace = s.active_workspace_or_err(client_id)?;

    // Discovery walks *upward*, so a root nested inside a repo reports the repo's top level and
    // several roots in one repo collapse onto one entry.
    let mut repos: Vec<GitRepoInfo> = Vec::new();
    for root in &workspace.paths {
        let Some(identity) = crate::git::discover_repo(root) else {
            continue;
        };
        let id = path_string(&identity.workdir);
        if !repos.iter().any(|r| r.repo_id == id) {
            repos.push(repo_info(&identity));
        }
    }

    // A repo can also sit *below* a root — a vendored subrepo under a non-repo workspace root —
    // which an upward walk from the root never sees. Open buffers find those, via the repo they
    // already name: a baseline's workdir, or a virtual buffer's source revision.
    //
    // Virtual buffers count because they outlive what produced them. Reading a commit from a repo
    // reached only through a file, then closing that file, must not strand the commit view in a
    // workspace that no longer admits its repo exists — every git read taken from that buffer
    // resolves through here.
    let mut nested: Vec<GitRepoInfo> = Vec::new();
    for buffer_id in s.buffers_in_workspace(&workspace.id) {
        let Some(workdir) = buffer_repo_id(s, buffer_id).map(std::path::PathBuf::from) else {
            continue;
        };
        let id = path_string(&workdir);
        if repos.iter().chain(nested.iter()).any(|r| r.repo_id == id) {
            continue;
        }
        if let Some(identity) = crate::git::discover_repo(&workdir) {
            nested.push(repo_info(&identity));
        }
    }
    // Buffer iteration order is a `HashMap`'s, so sort for a stable response.
    nested.sort_by(|a, b| a.repo_id.cmp(&b.repo_id));
    repos.append(&mut nested);

    // Which roots reach each repo, in either containment direction: a root inside the repo (the
    // ordinary case, and the subdirectory-root case) and a repo inside a root (the vendored one).
    for repo in &mut repos {
        let workdir = Path::new(&repo.repo_id);
        repo.roots = workspace
            .paths
            .iter()
            .filter(|root| root.starts_with(workdir) || workdir.starts_with(root))
            .map(|root| path_string(root))
            .collect();
    }

    Ok(repos)
}

/// Write the commit-message template and report where it landed, so the client can open it as an
/// ordinary buffer.
pub async fn git_prepare_commit(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: GitPrepareCommitParams,
) -> Result<GitPrepareCommitResult, RpcError> {
    let (repo_id, workdir, git_dir) = {
        let s = state.lock().await;
        let repo =
            resolve_writable_repo(&s, ctx.client_id, params.repo_id.as_ref(), params.buffer_id)?;
        (
            repo.repo_id.clone(),
            std::path::PathBuf::from(&repo.repo_id),
            std::path::PathBuf::from(&repo.git_dir),
        )
    };

    // Off the lock: status walks the working tree, and reading the previous message opens the repo.
    let (staged, template, conflicts) = tokio::task::spawn_blocking({
        let workdir = workdir.clone();
        let git_dir = git_dir.clone();
        let amend = params.amend;
        move || {
            let staged = crate::git::staged_files(&workdir);
            let template = commit_template(&workdir, &git_dir, &staged, amend);
            (staged, template, crate::git::conflicted_paths(&workdir))
        }
    })
    .await
    .map_err(|e| RpcError::internal(format!("preparing commit message: {e}")))?;

    // Refuse before opening a message buffer for a commit git is certain to reject. Git's own
    // refusal ("Committing is not possible because you have unmerged files") arrives after the user
    // has written the message, and names the state rather than the files.
    if !conflicts.is_empty() {
        return Ok(GitPrepareCommitResult {
            repo_id,
            blocked_by_conflicts: conflicts,
            ..Default::default()
        });
    }

    // `COMMIT_EDITMSG` in the repo's *own* git dir — per-worktree, so two worktrees compose their
    // messages independently. It's also where git itself keeps this file, so an abandoned message
    // is recoverable exactly where a user would look for it.
    let path = git_dir.join("COMMIT_EDITMSG");
    std::fs::write(&path, &template).map_err(RpcError::file_io)?;

    Ok(GitPrepareCommitResult {
        repo_id,
        path: path_string(&path),
        blocked_by_conflicts: Vec::new(),
        staged: staged
            .into_iter()
            .map(|(path, status)| StagedFile {
                path,
                status: status.to_string(),
            })
            .collect(),
    })
}

/// Which repo a commit is for: the explicit `repo_id`, else the repo of the buffer the user is
/// looking at, else the workspace's only writable repo. Ambiguity is an error rather than a guess
/// — committing to the wrong repo is not recoverable by pressing undo.
/// How far the log walk goes before giving up: **commits examined**, not rows produced. The
/// whole-repo walk makes those the same, but the file-scoped one diffs each commit against its
/// parent, so a narrow path in a deep history could walk indefinitely to fill one screen.
///
/// 20k is well past what anyone scrolls and costs tens of milliseconds for the repo log. The
/// picker reports hitting it (`PickerViewResult::truncated`) rather than pretending the history
/// ended, because the query filters what was loaded.
pub const LOG_MAX_EXAMINED: usize = 20_000;

/// The repo a buffer itself names, if any: the working directory its Git baseline resolved (a
/// file), or the repo a virtual buffer was materialised from (a commit or stash view, which has no
/// path but does know where its content came from).
///
/// Baseline first. A virtual buffer never has one, so the order only matters if a buffer somehow
/// had both, and the baseline is the live fact where the revision is a historical one.
fn buffer_repo_id(s: &ServerState, buffer_id: BufferId) -> Option<RepoId> {
    let from_baseline = s
        .git_baseline
        .get(&buffer_id)
        .and_then(|b| b.repo.as_ref())
        .map(|r| path_string(&r.workdir));
    from_baseline.or_else(|| {
        s.try_doc_of(buffer_id)
            .and_then(|d| d.virtual_source.as_ref())
            .map(|v| v.target.repo_id.clone())
    })
}

/// Repo resolution for a **read**: the repo the buffer names, and nothing else.
///
/// The buffer is the whole rule. An earlier version fell back to the workspace's only repo when the
/// buffer couldn't answer — which meant `Space g l` on a scratch buffer quietly picked a repo, and
/// meant multi-repo workspaces had a separate "ambiguous, say which one" failure the client had no
/// way to answer. Both are gone: a repo comes from the file you are looking at, or the command
/// refuses and says so. That refusal is the feature — it names the repo by pointing at what's on
/// screen, which is the one place the answer is never in doubt.
///
/// Wider than [`resolve_writable_repo`] only in what it accepts once resolved: a repo reachable
/// solely through an open buffer stays fully read-eligible (gutter, blame, log all work), because
/// nothing a read offers can mutate it.
pub fn resolve_readable_repo(
    s: &ServerState,
    client_id: ClientId,
    buffer_id: Option<BufferId>,
) -> Result<GitRepoInfo, RpcError> {
    let Some(buffer_id) = buffer_id else {
        return Err(RpcError::repo_needs_file());
    };
    let Some(workdir) = buffer_repo_id(s, buffer_id) else {
        // A buffer with a path that resolved no repo is *outside* one; a buffer without a path
        // hasn't got as far as having somewhere to look. Different remedies, so different wording.
        let has_path = s
            .try_doc_of(buffer_id)
            .is_some_and(|d| d.canonical_path.is_some());
        return Err(if has_path {
            RpcError::not_in_repo()
        } else {
            RpcError::repo_needs_file()
        });
    };
    reachable_repos(s, client_id)?
        .into_iter()
        .find(|r| r.repo_id == workdir)
        .ok_or_else(RpcError::not_in_repo)
}

/// Repo resolution for a **write**: [`resolve_readable_repo`], then the requirement that the repo
/// be one the workspace actually opened.
///
/// An explicit `repo_id` short-circuits the buffer entirely — that's how the branch picker's rows
/// act on the repo they were listed for, rather than on whatever buffer happens to be focused
/// behind the picker.
pub fn resolve_writable_repo(
    s: &ServerState,
    client_id: ClientId,
    repo_id: Option<&RepoId>,
    buffer_id: Option<BufferId>,
) -> Result<GitRepoInfo, RpcError> {
    let repo = match repo_id {
        Some(id) => resolve_repo(s, client_id, id)?,
        None => resolve_readable_repo(s, client_id, buffer_id)?,
    };
    require_workspace_repo(&repo)?;
    Ok(repo)
}

/// git's own commit template: the message — empty, the previous one when amending, or the one the
/// stopped operation prepared — followed by a comment block that `--cleanup=strip` discards.
/// Deliberately shaped like the one a terminal `git commit` produces: this is a file users have
/// read a thousand times.
///
/// **Mid-operation the message is already written, and using it is the whole point.** A merge (and
/// a conflicted cherry-pick or revert) leaves `MERGE_MSG`; a rebase leaves its commit's message in
/// `rebase-merge/message`. Seeding from them means concluding an operation shows what is about to
/// be committed and lets it be edited first — which is exactly what `git rebase --continue` hides
/// behind `$EDITOR`. Without this, `Space g c` mid-merge would overwrite git's own merge message
/// with a blank one.
///
/// Only the merge backend's rebase path is read (`rebase-merge/`); `--apply` keeps its state
/// elsewhere and falls back to the blank template. The merge backend has been git's default since
/// 2.26, and the fallback is a blank message, not a wrong one.
fn commit_template(
    workdir: &std::path::Path,
    git_dir: &std::path::Path,
    staged: &[(String, &str)],
    amend: bool,
) -> String {
    let mut out = String::new();
    let seed = if amend {
        crate::git::head_message(workdir)
    } else {
        std::fs::read_to_string(git_dir.join("MERGE_MSG"))
            .or_else(|_| std::fs::read_to_string(git_dir.join("rebase-merge/message")))
            .ok()
    };
    if let Some(seed) = seed {
        out.push_str(seed.trim_end());
        out.push('\n');
    }
    out.push('\n');
    out.push_str("# Please enter the commit message for your changes. Lines starting\n");
    out.push_str("# with '#' will be ignored, and an empty message aborts the commit.\n");
    out.push_str("#\n");
    match crate::git::discover_repo(workdir).map(|r| r.head) {
        Some(aether_protocol::git::GitHead::Branch { name, .. })
        | Some(aether_protocol::git::GitHead::Unborn { name }) => {
            out.push_str(&format!("# On branch {name}\n"));
        }
        Some(aether_protocol::git::GitHead::Detached { oid }) => {
            out.push_str(&format!("# HEAD detached at {oid}\n"));
        }
        None => {}
    }
    if amend {
        out.push_str("#\n# Amending the previous commit.\n");
    }
    if staged.is_empty() {
        out.push_str("#\n# No changes staged for commit.\n");
    } else {
        out.push_str("#\n# Changes to be committed:\n");
        for (path, status) in staged {
            // Git pads the status word to a fixed column; matching it keeps the block aligned.
            out.push_str(&format!("#\t{status:<12}{path}\n"));
        }
    }
    out
}

/// Run the real `git commit`, then reconcile the repo.
pub async fn git_commit(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: GitCommitParams,
) -> Result<GitCommitResult, RpcError> {
    let (workdir, git_dir) = {
        let s = state.lock().await;
        let repo = resolve_repo(&s, ctx.client_id, &params.repo_id)?;
        require_workspace_repo(&repo)?;
        (
            std::path::PathBuf::from(&repo.repo_id),
            std::path::PathBuf::from(&repo.git_dir),
        )
    };
    let message_path = git_dir.join("COMMIT_EDITMSG");
    if !message_path.exists() {
        return Err(RpcError::internal(
            "no prepared commit message — call git/prepare_commit first",
        ));
    }

    // An empty message is an *abandon*, not a failure — git's own rule ("Aborting commit due to
    // empty commit message"), and the one that makes closing the buffer without writing anything
    // behave like quitting an editor without writing. Answered here rather than by running git and
    // reading its complaint: the client needs to tell "you changed your mind" (close quietly) from
    // "a hook said no" (keep the message and let them retry), and that distinction must not come
    // from parsing stderr.
    //
    // Emptiness follows `--cleanup=strip` below: comment lines and blank lines are not content.
    let is_empty = std::fs::read_to_string(&message_path).is_ok_and(|text| {
        text.lines()
            .all(|line| line.trim().is_empty() || line.trim_start().starts_with('#'))
    });
    if is_empty {
        return Ok(GitCommitResult {
            empty_message: true,
            ..Default::default()
        });
    }

    // Suppress before the spawn, not after: `git commit` writes the index and HEAD, and a
    // `pre-commit` hook may rewrite working-tree files. Every one of those events belongs to the
    // reconciliation below, which is also what makes its report a true account (see
    // `ServerState::git_suppressed`).
    state.lock().await.git_suppressed.insert(workdir.clone());

    let mut args: Vec<&str> = vec!["commit", "--cleanup=strip", "-F"];
    let message_arg = path_string(&message_path);
    args.push(&message_arg);
    if params.amend {
        args.push("--amend");
    }
    let outcome = crate::git_cli::run(&workdir, &args).await;

    let mut s = state.lock().await;
    s.git_suppressed.remove(&workdir);

    let output = match outcome {
        Ok(o) => o,
        Err(e) => {
            drop(s);
            return Err(RpcError::internal(format!("running git commit: {e}")));
        }
    };

    // A refusal is an outcome, not an error: a failing `pre-commit` hook is the system working.
    // Reconcile either way — a hook that rewrote files then failed still moved the tree.
    let (refreshed, pushes) = reconcile_repo(&mut s, &workdir);
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    reconcile_working_changes(state, &workdir).await;

    if !output.success() {
        // git puts "nothing to commit" on stdout and hook failures on stderr; the user needs
        // whichever one it chose.
        let message = if output.stderr.trim().is_empty() {
            output.stdout
        } else {
            output.stderr
        };
        return Ok(GitCommitResult {
            empty_message: false,
            commit: None,
            message: message.trim_end().to_string(),
            refreshed,
            ..Default::default()
        });
    }

    let commit = tokio::task::spawn_blocking({
        let workdir = workdir.clone();
        move || {
            crate::git::commit_info(
                &crate::git::GitRepo {
                    workdir,
                    rel_path: std::path::PathBuf::new(),
                },
                "HEAD",
            )
        }
    })
    .await
    .ok()
    .flatten();

    // Concluding what the commit was part of. A stopped *merge* is over the moment it's committed
    // (git clears `MERGE_HEAD` itself), but a rebase — and a multi-commit cherry-pick or revert —
    // still has patches to apply, and leaving the user to go and find `--continue` in a terminal is
    // exactly the gap that made a conflicted pull a dead end. Nothing to do when the repo is clean,
    // which is every ordinary commit.
    let (continued, conflicts) = continue_stopped_operation(state, &workdir).await?;

    Ok(GitCommitResult {
        commit,
        message: String::new(),
        refreshed,
        empty_message: false,
        operation: continued,
        conflicts,
    })
}

/// If `workdir` is still stopped in an operation, run its `--continue` and report what happened.
///
/// Returns the operation that was resumed (`None` when there was none) and any paths the resumed
/// operation left conflicted — the next patch can stop exactly as the first one did.
///
/// Runs through [`run_tree_git`] because `--continue` applies patches: it moves the working tree,
/// and skipping the watcher suppression or the reconciliation would leave every open buffer stale.
/// Never needs an editor — the commit has already been made, and [`crate::git_cli`] forecloses the
/// question anyway.
async fn continue_stopped_operation(
    state: &SharedState,
    workdir: &Path,
) -> Result<(Option<GitRepoOperation>, Vec<String>), RpcError> {
    let operation = {
        let workdir = workdir.to_path_buf();
        tokio::task::spawn_blocking(move || crate::git::repo_operation(&workdir))
            .await
            .map_err(|e| RpcError::internal(format!("reading repo state: {e}")))?
    };
    let Some(operation) = operation else {
        return Ok((None, Vec::new()));
    };
    let verb = match operation {
        GitRepoOperation::Rebase => "rebase",
        GitRepoOperation::CherryPick => "cherry-pick",
        GitRepoOperation::Revert => "revert",
        GitRepoOperation::ApplyMailbox => "am",
        // A merge is concluded by the commit itself, and `git merge --continue` on a repo whose
        // `MERGE_HEAD` has just been cleared is an error. Bisect isn't a thing to continue at all.
        GitRepoOperation::Merge | GitRepoOperation::Bisect => {
            return Ok((Some(operation), Vec::new()))
        }
    };
    run_tree_git(state, workdir, &[verb, "--continue"], None, None).await?;

    let conflicts = {
        let workdir = workdir.to_path_buf();
        tokio::task::spawn_blocking(move || crate::git::conflicted_paths(&workdir))
            .await
            .unwrap_or_default()
    };
    Ok((Some(operation), conflicts))
}

/// `<op> --abort`: abandon a stopped merge / rebase / cherry-pick / revert and put the tree back.
pub async fn git_abort_operation(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: GitAbortOperationParams,
) -> Result<GitAbortOperationResult, RpcError> {
    let (workdir, blocked) = {
        let s = state.lock().await;
        let repo =
            resolve_writable_repo(&s, ctx.client_id, params.repo_id.as_ref(), params.buffer_id)?;
        let workdir = std::path::PathBuf::from(&repo.repo_id);
        let blocked = dirty_buffers_in_repo(&s, &workdir);
        (workdir, blocked)
    };

    let operation = {
        let workdir = workdir.clone();
        tokio::task::spawn_blocking(move || crate::git::repo_operation(&workdir))
            .await
            .map_err(|e| RpcError::internal(format!("reading repo state: {e}")))?
    };
    let Some(operation) = operation else {
        return Ok(GitAbortOperationResult {
            status: GitAbortStatus::NothingInProgress,
            ..Default::default()
        });
    };
    // An abort resets the working tree, so an unsaved buffer would lose its edits — the same
    // pre-flight checkout, stash and pull make, for the same reason.
    if !blocked.is_empty() {
        return Ok(GitAbortOperationResult {
            status: GitAbortStatus::BlockedByDirtyBuffers,
            operation: Some(operation),
            blocked,
            ..Default::default()
        });
    }

    let verb = match operation {
        GitRepoOperation::Merge => "merge",
        GitRepoOperation::Rebase => "rebase",
        GitRepoOperation::CherryPick => "cherry-pick",
        GitRepoOperation::Revert => "revert",
        GitRepoOperation::ApplyMailbox => "am",
        // `git bisect` is `reset`, not `abort`, and nothing here starts one — but a user who began
        // one in a terminal still deserves the way out this key promises.
        GitRepoOperation::Bisect => "bisect",
    };
    let arg = if matches!(operation, GitRepoOperation::Bisect) {
        "reset"
    } else {
        "--abort"
    };
    let run = run_tree_git(state, &workdir, &[verb, arg], None, None).await?;

    if !run.output.success() {
        return Ok(GitAbortOperationResult {
            status: GitAbortStatus::Refused,
            operation: Some(operation),
            message: git_failure_message(&run.output),
            refreshed: run.refreshed,
            ..Default::default()
        });
    }
    Ok(GitAbortOperationResult {
        status: GitAbortStatus::Aborted,
        operation: Some(operation),
        refreshed: run.refreshed,
        ..Default::default()
    })
}

/// Move HEAD back, keeping the index and working tree (`git reset --soft`). See [`GitReset`] for
/// why this is soft-only.
pub async fn git_reset(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: GitResetParams,
) -> Result<GitResetResult, RpcError> {
    let workdir = {
        let s = state.lock().await;
        let repo =
            resolve_writable_repo(&s, ctx.client_id, params.repo_id.as_ref(), params.buffer_id)?;
        std::path::PathBuf::from(&repo.repo_id)
    };

    // Resolved *before* the reset, while the commits are still reachable from HEAD — afterwards
    // there is nothing to walk from.
    let undone = tokio::task::spawn_blocking({
        let workdir = workdir.clone();
        let rev = params.rev.clone();
        move || crate::git::commits_between(&workdir, "HEAD", &rev)
    })
    .await
    .unwrap_or_default();

    state.lock().await.git_suppressed.insert(workdir.clone());
    let outcome = crate::git_cli::run(&workdir, &["reset", "--soft", &params.rev]).await;
    let mut s = state.lock().await;
    s.git_suppressed.remove(&workdir);

    let output = match outcome {
        Ok(o) => o,
        Err(e) => {
            drop(s);
            return Err(RpcError::internal(format!("running git reset: {e}")));
        }
    };

    // Reconcile regardless: a soft reset moves no files, but it moves HEAD, and every open
    // buffer's baseline is measured against HEAD.
    let (_, pushes) = reconcile_repo(&mut s, &workdir);
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    reconcile_working_changes(state, &workdir).await;

    if !output.success() {
        let message = if output.stderr.trim().is_empty() {
            output.stdout
        } else {
            output.stderr
        };
        return Ok(GitResetResult {
            head: None,
            undone: Vec::new(),
            message: message.trim_end().to_string(),
        });
    }

    let head = tokio::task::spawn_blocking({
        let workdir = workdir.clone();
        move || {
            crate::git::commit_info(
                &crate::git::GitRepo {
                    workdir,
                    rel_path: std::path::PathBuf::new(),
                },
                "HEAD",
            )
        }
    })
    .await
    .ok()
    .flatten();

    Ok(GitResetResult {
        head,
        undone,
        message: String::new(),
    })
}

/// Mutating git operations require a repo the *workspace* reaches, not merely one an open buffer
/// wandered into. See `GitRepoInfo::roots`.
fn require_workspace_repo(repo: &GitRepoInfo) -> Result<(), RpcError> {
    if repo.roots.is_empty() {
        return Err(RpcError::repo_not_writable(&repo.repo_id));
    }
    Ok(())
}

/// Point a repo's diff baseline at a revision or at the files on disk, or (with `source: None`)
/// back at the index.
///
/// Repo-scoped: every open buffer in the repo re-resolves, so the gutter answers one consistent
/// question as you move between files. A revision is resolved to a commit *once*, here, and the
/// pinned commit is what every later baseline load uses — see [`crate::git::BaselineChoices`] for
/// why a moving baseline would be worse than a slightly stale one.
pub async fn git_set_baseline(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: GitSetBaselineParams,
) -> Result<GitSetBaselineResult, RpcError> {
    let mut s = state.lock().await;
    let repo = resolve_repo(&s, ctx.client_id, &params.repo_id)?;
    let workdir = std::path::PathBuf::from(&repo.repo_id);

    let baseline = match &params.source {
        Some(GitBaselineChoice::Rev { rev }) => {
            // Resolved before anything is stored, so a typo leaves the previous baseline intact
            // rather than silently dropping the user back to the index.
            let commit = crate::git::resolve_rev(&workdir, rev)
                .ok_or_else(|| RpcError::unknown_revision(rev))?;
            let pinned = GitBaselineSource::Rev {
                label: rev.clone(),
                commit,
            };
            s.git_baseline_choices
                .insert(workdir.clone(), pinned.clone());
            Some(pinned)
        }
        Some(GitBaselineChoice::Saved) => {
            // Nothing to resolve: the content is each document's own snapshot, taken when it went
            // dirty. A repo with no dirty buffers simply shows no changes, which is the truth.
            s.git_baseline_choices
                .insert(workdir.clone(), GitBaselineSource::Saved);
            Some(GitBaselineSource::Saved)
        }
        None => {
            s.git_baseline_choices.remove(&workdir);
            None
        }
    };

    let mut buffers: Vec<BufferId> = s
        .git_baseline
        .iter()
        .filter(|(_, b)| b.repo.as_ref().is_some_and(|r| r.workdir == workdir))
        .map(|(id, _)| *id)
        .collect();
    buffers.sort_unstable();

    let mut pushes: PendingPushes = Vec::new();
    for id in &buffers {
        pushes.extend(refresh_git_for_buffer(&mut s, *id));
    }

    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    // The working-changes view is measured against the baseline too, so it moves with the gutters
    // rather than waiting to be re-shown. Not in `buffers` above: that list is the *file* buffers
    // whose diff was recomputed, and a patch buffer has no `GitBaseline` to be found by.
    refresh_working_changes_views(state, &std::iter::once(workdir).collect()).await;
    Ok(GitSetBaselineResult { baseline, buffers })
}

/// What a cold `workspace/activate` reads before taking the state lock: the base workspace's name,
/// the roots to install (already materialised through any variant bindings), its declared projects,
/// and — for a variant only — the *base* roots those were remapped from.
pub type ColdLoad = (
    String,
    Vec<std::path::PathBuf>,
    Vec<crate::config::ProjectRef>,
    Option<Vec<std::path::PathBuf>>,
);

/// Bind (or unbind) a repo to a worktree in a workspace variant, then activate the result — see
/// [`WorkspaceBindWorktree`].
pub async fn workspace_bind_worktree(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: WorkspaceBindWorktreeParams,
) -> Result<WorkspaceActivateResult, RpcError> {
    let client_id = ctx.client_id;
    let (context_id, family, bindings) = {
        let s = state.lock().await;
        let context_id = match params.workspace.clone() {
            Some(id) => id,
            None => s.active_workspace_or_err(client_id)?.id.clone(),
        };
        let repo = resolve_writable_repo(&s, client_id, params.repo_id.as_ref(), None)?;
        // Bindings are keyed by the repo **family** — its common dir — never by a workdir. Standing
        // in a workspace already bound to `feature`, the active repo *is* that worktree, and a
        // workdir key would write a second binding for the same repo and leave the first to fight
        // it. The common dir is what every worktree of a family shares, so that hazard is
        // unrepresentable rather than guarded against: there is one key per repo by construction.
        //
        // This is why the normalisation step that used to sit here is gone. It existed only to
        // recover "the workdir a configured root discovers" from the family, which is precisely the
        // work a wrong key forces on every call site.
        let family = std::path::PathBuf::from(&repo.common_dir);
        // Reachability is still checked, and still refused rather than approximated:
        // `resolve_writable_repo` can reach a repo through a buffer outside every root, and a
        // binding for a family no root reaches would persist, report success, remap nothing, and be
        // swept away as unreachable on the next load.
        let configured = configured_workspace_roots(&s, &context_id)?;
        if !crate::worktree::reachable_families(&configured).contains(&family) {
            return Err(RpcError::repo_not_found(&repo.repo_id));
        }
        // The set we are editing is the one *this context* is resolved against, read off its entry
        // — not the workspace's, which no longer has a single one. Two windows in two contexts each
        // change their own.
        (context_id.clone(), family, loaded_bindings(&s, &context_id))
    };
    // An ephemeral context has no config file, so it has no configured roots to remap and nothing
    // to persist a binding against.
    if aether_protocol::is_ephemeral_workspace_id(&context_id) {
        return Err(RpcError::invalid_params(
            "worktrees need a configured workspace",
        ));
    }
    let name = crate::worktree::context_workspace_name(&context_id).to_string();

    let mut bindings = bindings;
    // Structural cleanup on the way through: a binding whose repo no longer sits under any
    // configured root is unreachable, and the context should not keep carrying it. This is the
    // machine-state counterpart of the config nesting `projects` inside their root — there a
    // dangling reference is unrepresentable, here it is representable, so it is pruned.
    {
        let s = state.lock().await;
        let configured = configured_workspace_roots(&s, &context_id)?;
        drop(s);
        crate::worktree::prune_unreachable_bindings(&configured, &mut bindings);
    }
    if params.worktree.is_empty() {
        bindings.remove(&family);
    } else {
        bindings.insert(family.clone(), params.worktree.clone());
    }

    // There is no target to choose: the bindings belong to the workspace we are standing in, and
    // that is where we stay. Binding used to spawn a second workspace id (`aether/feature-auth`) the
    // first time it was called from an unbound workspace, which made one keypress mean "adjust this"
    // or "create and leave" depending on state that wasn't on screen. Having two trees of one repo
    // open at once is a second workspace, made deliberately — not a side effect of pressing Enter.
    //
    // Persist before activating: activation materialises roots *from* the bindings, so a write that
    // landed after it would open the old shape.
    // Where the caller is looking, resolved to a repo-relative path *before* we move — so the same
    // file can be opened on the other tree afterwards. `open_last` alone lands on the context's MRU
    // head, which is only incidentally the buffer you were in.
    // What to land on in the new context. `open_last` alone would land on *its* MRU head, which for
    // a context never visited is nothing at all — so switching would drop you on a scratch rather
    // than on the file you were reading. The caller names the buffer when it has one; otherwise the
    // head of the context we are leaving is what "the file you were on" means.
    //
    // This is where switching differs from the rebuild-in-place it replaced: that one *moved* your
    // buffers, so the MRU head was already the file on the new tree. Contexts keep their own buffer
    // lists — which is the point — so the follow has to be arranged rather than inherited.
    let viewing = match params.buffer_id {
        Some(id) => Some(id),
        None => {
            let s = state.lock().await;
            s.mru_buffer(&context_id)
        }
    };
    let landing = match viewing {
        Some(id) => buffer_location_in_context(state, &context_id, id).await,
        None => None,
    };

    // Moving between contexts is a *switch*, not a rebuild: the context you are leaving stays
    // loaded with its own buffers, index and MRU, and the one you are entering is looked up or
    // created from the bindings. That is the whole point of keying entries by their binding set —
    // it is what lets two windows sit in two trees of one repo at once, and it is why this no
    // longer mutates the workspace under everyone else standing in it.
    let result_bindings = bindings.clone();
    let result_context = crate::worktree::context_id(&name, &bindings);
    let mut result = activate_context(
        state,
        ctx,
        name,
        bindings,
        params.open_last && landing.is_none(),
    )
    .await?;
    if params.open_last {
        if let Some(loc) = landing {
            // Best-effort, deliberately: the file you were on may simply not exist on the other
            // tree — a new branch, or an untracked file that never left the checkout. Propagating
            // that would fail the whole switch over the *landing*, leaving you neither moved nor
            // told why. So a landing that can't be opened falls back to the ordinary one.
            match view_open(
                state,
                ctx,
                ViewOpenParams {
                    path_index: Some(loc.path_index),
                    relative_path: Some(loc.relative_path),
                    ..Default::default()
                },
            )
            .await
            {
                Ok(opened) => result.opened = Some(opened),
                Err(_) => {
                    let mut fallback = activate_context(
                        state,
                        ctx,
                        crate::worktree::context_workspace_name(&result_context).to_string(),
                        result_bindings,
                        true,
                    )
                    .await?;
                    result.opened = fallback.opened.take();
                    result.last_view_id = fallback.last_view_id;
                }
            }
        }
    }
    Ok(result)
}

/// `path` as a root index + root-relative path in `workspace_id`. The client-scoped
/// [`workspace_location_of`] asks the same question of whatever workspace a client is standing in;
/// this one names the workspace, for a caller that has just changed its shape.
/// Where a buffer sits in a context's roots, as `(root index, relative path)`.
///
/// Used to carry the file you are looking at across a context switch, and it needs no re-joining to
/// do it: [`crate::worktree::materialise_roots`] preserves root **count and order**, so index `i`
/// in one context is index `i` in another. The location resolved against the context you are
/// leaving is directly valid in the one you are entering.
async fn buffer_location_in_context(
    state: &SharedState,
    context_id: &str,
    buffer_id: BufferId,
) -> Option<aether_protocol::buffer::BufferLocation> {
    let path = {
        let s = state.lock().await;
        s.try_doc_of(buffer_id)?.canonical_path.clone()?
    };
    workspace_location_of_workspace(state, context_id, &path).await
}

async fn workspace_location_of_workspace(
    state: &SharedState,
    workspace_id: &str,
    path: &Path,
) -> Option<aether_protocol::buffer::BufferLocation> {
    let s = state.lock().await;
    let entry = s.workspaces.get(workspace_id)?;
    entry.paths.iter().enumerate().find_map(|(i, root)| {
        path.strip_prefix(root)
            .ok()
            .map(|rel| aether_protocol::buffer::BufferLocation {
                path_index: i as u32,
                relative_path: rel.to_string_lossy().into_owned(),
            })
    })
}

/// Rebuild an already-loaded workspace around freshly-changed bindings, carrying its open buffers
/// across. Returns the buffers that stayed behind because they were unsaved.
///
/// The in-place case: the roots move, so every buffer under a remapped root has to
/// be reopened at the same *relative* path in the new tree. Left alone they would silently keep
/// editing the other worktree — the `git-worktree.nvim` #88 failure, tractable for us only because
/// the server owns the buffer↔workspace association and can do the whole switch under one lock.
///
/// **Dirty buffers stay behind and do not block the switch.** `workspace/remove_root` refuses on
/// them because it is *destroying* a context; a rebind destroys nothing, and a dirty buffer left in
/// the previous shape is exactly what you find when you switch back.
///
/// **A workspace is one thing, however many clients are in it.** Its roots move for all of them, so
/// every *other* client on it is told what closed and what replaced it (`view/closed`), the same
/// way `workspace/remove_root` and `workspace/delete` tell them. `client_id` is the initiator, who
/// is excluded — its own reply carries the new state.
pub async fn rebind_loaded_workspace(
    state: &SharedState,
    client_id: ClientId,
    workspace_id: &str,
    was_viewing: Option<BufferId>,
) -> Result<RebindOutcome, RpcError> {
    let (bindings, configured) = {
        let s = state.lock().await;
        (
            loaded_bindings(&s, workspace_id),
            configured_workspace_roots(&s, workspace_id)?,
        )
    };
    let bindings_now_empty = bindings.is_empty();
    let (roots, _unresolved) = {
        let configured = configured.clone();
        tokio::task::spawn_blocking(move || {
            crate::worktree::materialise_roots(&configured, &bindings)
        })
        .await
        .map_err(|e| RpcError::internal(format!("resolving worktree bindings: {e}")))?
    };

    let mut s = state.lock().await;
    let Some(entry) = s.workspaces.get(workspace_id) else {
        return Ok(RebindOutcome::default());
    };
    let old_roots = entry.paths.clone();
    if old_roots == roots {
        return Ok(RebindOutcome::default());
    }

    // Which open buffers move, and where to. A buffer under no remapped root doesn't move at all —
    // that's the shared non-repo root case, and it keeps its document, rope and undo stack.
    let mut follow: Vec<(BufferId, std::path::PathBuf)> = Vec::new();
    let mut stayed: Vec<BufferId> = Vec::new();
    // Where a buffer that *stayed* would have gone, for the landing below. Not a `view/closed`
    // successor — nothing closed — so it is kept apart from `successor`.
    let mut stayed_successor: std::collections::HashMap<BufferId, std::path::PathBuf> =
        Default::default();
    let mut close: Vec<BufferId> = Vec::new();
    for id in s.buffers_in_workspace(workspace_id) {
        let Some(doc) = s.try_doc_of(id) else {
            continue;
        };
        let Some(path) = doc.canonical_path.clone() else {
            continue; // a scratch has no path to remap
        };
        let Some(mapped) = remap_path(&path, &old_roots, &roots) else {
            continue; // outside every moved root
        };
        if doc.dirty {
            // Unsaved work stays where it was edited — but the *view* still moves. Recording
            // the mapping without closing anything means a client that was looking at this buffer
            // lands on the same file in the new tree, with its unsaved copy left open behind it.
            // Without this the landing fell back to the MRU head, which is this very buffer: you
            // asked to switch trees and stayed on the old tree's file, now shown as an absolute
            // path because it sits outside every root. That is the `git-worktree.nvim` #88 failure
            // wearing a different hat.
            if mapped.exists() {
                stayed_successor.insert(id, mapped);
            }
            stayed.push(id);
            continue;
        }
        close.push(id);
        if mapped.exists() {
            // Paired with the buffer it replaces, so every *other* client viewing that buffer can
            // be handed the same file on the new tree rather than an arbitrary survivor.
            follow.push((id, mapped));
        }
    }
    // Captured BEFORE teardown, which drops the viewports and MRU entries this reads. Without it
    // the other clients on this workspace keep buffer ids the server has just closed, and every
    // request they make on one comes back `unknown buffer_id` — the workspace moved under them
    // with nothing said.
    let affected = clients_affected_by_close(&s, &close, client_id);
    for id in &close {
        s.close_buffer(*id);
    }

    // The followed set becomes the workspace's dormant list: listed in the picker, materialised
    // when opened. Reuses the session-restore shape rather than eagerly opening N files, which on a
    // wide open set would stall the switch for no benefit.
    //
    // Ids come from the allocator first, because it borrows `s` mutably on its own and the entry
    // does too.
    // Keyed by the buffer it replaces, and carried as a **path**: the dormant id below is not a
    // stable handle for another client (see `ViewClosedParams::next_path`).
    let mut successor: std::collections::HashMap<BufferId, std::path::PathBuf> = Default::default();
    let dormant: Vec<crate::state::DormantView> = follow
        .into_iter()
        .map(|(was, path)| {
            successor.insert(was, path.clone());
            let id = s.allocate_buffer_id();
            crate::state::DormantView {
                id,
                view: s.allocate_view_id(),
                // A file that followed a worktree switch comes back as its editor.
                kind: None,
                source: crate::state::DormantSource::File(path),
            }
        })
        .collect();
    if let Some(entry) = s.workspaces.get_mut(workspace_id) {
        entry.paths = roots.clone();
        entry.workspace_index =
            Arc::new(crate::workspace_index::WorkspaceIndex::new(roots.clone()));
        // Dormant entries that were already here are **remapped, not discarded** — a buffer listed
        // from a restored session but never opened is still in the picker, and replacing the list
        // wholesale silently dropped it. Ones under a moved root follow to the same relative path
        // (keeping their reserved id, since nothing has materialised them); ones outside every
        // moved root, and scratches, are left exactly as they are.
        for d in &mut entry.dormant_views {
            if let crate::state::DormantSource::File(path) = &d.source {
                if let Some(mapped) = remap_path(path, &old_roots, &roots) {
                    d.source = crate::state::DormantSource::File(mapped);
                }
            }
        }
        // Then the buffers this rebind just closed, which are dormant now too.
        entry.dormant_views.extend(dormant);
    }
    // Both halves above can produce a path that is already listed — a remapped entry landing on a
    // path this rebind also followed, or one whose file another client has since opened live. The
    // list's invariant is restored once, here, rather than proved at each route in.
    s.dedupe_dormant(workspace_id);
    if let Some(entry) = s.workspaces.get_mut(workspace_id) {
        // The configured roots are what unbinding restores, so they are recorded the moment a
        // binding exists and dropped the moment none does — leave a stale `Some` here and the next
        // rebind would remap against a shape the workspace no longer has.
        entry.base_paths = (!bindings_now_empty).then(|| configured.clone());
    }
    let landing = was_viewing.and_then(|id| {
        successor
            .get(&id)
            .or_else(|| stayed_successor.get(&id))
            .cloned()
    });

    // Tell the other clients on this workspace. The new shape goes *first*, so their roots are
    // current before they open the successor below and resolve its path against them — otherwise
    // every label is computed against roots the workspace no longer has. Then what closed and what
    // replaced it: each is handed the same file on the new tree when it exists there (a dormant id,
    // materialised when they open it) and the workspace's usual successor when it doesn't. Built
    // after teardown so that fallback reflects the settled MRU.
    // The roots moved, so the servers pinned against the old ones are pinned to paths this
    // workspace no longer has — and on a `git worktree remove` those paths are about to stop
    // existing. Reconciling here rather than leaving it to the activation that usually follows:
    // `release_worktree_bindings` rebinds without activating, so that path had no reconcile at all
    // and left a rust-analyzer rooted in the directory git was about to delete.
    let launches = reconcile_workspace_pins(&mut s, workspace_id);
    let mut pushes = workspace_changed_pushes(&s, workspace_id, client_id);
    pushes.extend(refresh_view_pickers(&mut s));
    pushes.extend(refresh_lsp_server_pickers(&mut s));
    pushes.extend(buffer_closed_pushes_with(&s, &affected, &successor));
    let watcher = s.watcher.clone();
    drop(s);

    spawn_pinned_launches(state, launches);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    if let Some(w) = watcher {
        crate::watcher::unwatch_workspace_paths(&w, &old_roots);
        crate::watcher::watch_workspace_paths(&w, &roots);
    }
    Ok(RebindOutcome { stayed, landing })
}

/// What a rebind leaves the caller to act on.
#[derive(Default)]
pub struct RebindOutcome {
    /// Buffers that stayed behind because they were unsaved.
    pub stayed: Vec<BufferId>,
    /// Where the *initiating* client should land: the file it was viewing, on the new tree. `None`
    /// when it wasn't viewing anything remapped (a scratch, a root that didn't move, or a file that
    /// doesn't exist on the target) — the caller then falls back to the workspace's usual choice.
    /// Where a buffer that stayed behind would have gone. Retained for the caller that wants to
    /// land on it; the context switch computes its own landing before moving, from the roots it is
    /// leaving, because index `i` means the same root in both.
    #[allow(dead_code)]
    pub landing: Option<std::path::PathBuf>,
}

/// Resolve a workspace's live roots from its configured ones and its bindings, for activation.
///
/// Returns the roots to open plus the `base_paths` to record: `Some(configured)` for a bound
/// workspace — the shape its roots are a remapping *of*, and what unbinding restores — and `None`
/// for an unbound one, whose roots are already its configured ones.
///
/// `repair` gates the `git worktree repair` retry: worth it on a cold load from disk, skipped for an
/// in-memory registration, which has no repo that could have moved out from under it.
pub async fn materialise(
    workspace: &str,
    configured: Vec<std::path::PathBuf>,
    bindings: &std::collections::BTreeMap<std::path::PathBuf, String>,
    repair: bool,
) -> Result<(Vec<std::path::PathBuf>, Option<Vec<std::path::PathBuf>>), RpcError> {
    if bindings.is_empty() {
        return Ok((configured, None));
    }
    let (roots, unresolved) = if repair {
        materialise_with_repair(&configured, bindings.clone()).await?
    } else {
        crate::worktree::materialise_roots(&configured, bindings)
    };
    if !unresolved.is_empty() {
        tracing::warn!(
            workspace = %workspace,
            worktrees = ?unresolved,
            "worktree bindings could not be resolved; those roots fall back to the configured path"
        );
    }
    Ok((roots, Some(configured)))
}

/// Materialise bound roots, running `git worktree repair` once if any binding fails to
/// resolve and then trying again.
///
/// The centralised store means a repo and its worktrees never move together, so moving the repo
/// breaks every binding at once — and `repair` is exactly the command for that, rewriting the admin
/// entries to point where the trees actually are. Cheap to attempt and a no-op when nothing is
/// broken, but only attempted when something *is*: it is a mutation, and running it on every
/// activation would put a write in the read path.
///
/// Bindings that still don't resolve leave their root at the configured path. A workspace that
/// can't fully materialise degrades rather than refusing to open — refusing would leave no way
/// back in.
async fn materialise_with_repair(
    base_roots: &[std::path::PathBuf],
    bindings: std::collections::BTreeMap<std::path::PathBuf, String>,
) -> Result<(Vec<std::path::PathBuf>, Vec<String>), RpcError> {
    let first = {
        let base_roots = base_roots.to_vec();
        let bindings = bindings.clone();
        tokio::task::spawn_blocking(move || {
            crate::worktree::materialise_roots(&base_roots, &bindings)
        })
        .await
        .map_err(|e| RpcError::internal(format!("resolving worktree bindings: {e}")))?
    };
    if first.1.is_empty() {
        return Ok(first);
    }
    // Repair from each bound repo — the admin entries live in its common dir.
    for workdir in bindings.keys() {
        let _ = crate::git_cli::run(workdir, &["worktree", "repair"]).await;
    }
    let base_roots = base_roots.to_vec();
    tokio::task::spawn_blocking(move || crate::worktree::materialise_roots(&base_roots, &bindings))
        .await
        .map_err(|e| RpcError::internal(format!("resolving worktree bindings: {e}")))
}

/// A workspace's **configured** roots — what its TOML declares, canonicalized, in declaration
/// order. The shape its bindings remap, and what unbinding restores.
fn configured_workspace_roots(
    s: &ServerState,
    id: &str,
) -> Result<Vec<std::path::PathBuf>, RpcError> {
    // Prefers what is already loaded: a bound workspace carries its configured roots on the entry,
    // and an unbound one's `paths` are them. Only an unloaded workspace falls through to the TOML,
    // which keeps an interactive rebind off the disk — and lets an in-memory workspace (tests,
    // embeddings, one registered by `spawn_for_test`) work at all, since it has no file to read.
    if let Some(entry) = s.workspaces.get(id) {
        return Ok(entry.configured_paths().to_vec());
    }
    let dir = s
        .workspaces_dir()
        .map_err(|e| RpcError::internal(format!("resolving workspaces dir: {e}")))?;
    let cfg =
        crate::config::load_workspace_in(&dir, id).map_err(|_| RpcError::unknown_workspace(id))?;
    cfg.paths()
        .iter()
        .map(|p| crate::config::canonicalize_workspace_path(p))
        .collect::<Result<_, _>>()
        .map_err(|e| RpcError::invalid_path(format!("canonicalizing workspace path: {e}")))
}

/// `path`'s equivalent under the new root list: the same *relative* location beneath whichever root
/// contains it. `None` when no root does, or when that root didn't move.
fn remap_path(
    path: &Path,
    old_roots: &[std::path::PathBuf],
    new_roots: &[std::path::PathBuf],
) -> Option<std::path::PathBuf> {
    // Root lists are the same length and order by construction — that is what materialisation
    // preserves, and what lets projects' positional `root_index` survive a variant.
    for (old, new) in old_roots.iter().zip(new_roots) {
        if old == new {
            continue;
        }
        if let Ok(relative) = path.strip_prefix(old) {
            return Some(new.join(relative));
        }
    }
    None
}

/// The worktree bindings recorded for one workspace id — empty for an unbound workspace, and for
/// any id the session file has never seen.
///
/// Read straight off disk rather than cached in memory: bindings change rarely, are needed on
/// activation and on bind/unbind only, and the session file is the single source of truth for them
/// — a second copy in `ServerState` would be one more thing to keep in step.
pub fn worktree_bindings(
    sessions_path: Option<&Path>,
    workspace_name: &str,
    requested: Option<&std::collections::BTreeMap<std::path::PathBuf, String>>,
) -> std::collections::BTreeMap<std::path::PathBuf, String> {
    // An explicit set wins, including an explicit *empty* one — that is how a caller says "the base,
    // regardless of where I was last".
    if let Some(bindings) = requested {
        return bindings.clone();
    }
    // Nothing asked for: go where this workspace was last. Windows have no identity across a
    // restart, so this is the whole of "come back where I was" — and with two windows in two
    // contexts, both land in the more recent one, which is one keystroke to correct.
    let Some(path) = sessions_path else {
        return Default::default();
    };
    crate::config::load_workspace_sessions_at(path)
        .ok()
        .and_then(|s| {
            s.workspaces
                .get(workspace_name)
                .map(|e| e.most_recent_bindings())
        })
        .unwrap_or_default()
}

/// Every loaded context of the workspace named `workspace`, by entry id.
///
/// Workspace-level edits — adding a root, declaring a project, renaming — are edits to the
/// *workspace*, so they have to reach all of its contexts. A context left behind would be resolving
/// paths against a shape the workspace no longer has, which is exactly what `workspace/changed`
/// exists to stop happening to a second client.
pub fn contexts_of(s: &crate::state::ServerState, workspace: &str) -> Vec<String> {
    s.workspaces
        .iter()
        .filter(|(_, e)| e.name.as_deref() == Some(workspace))
        .map(|(id, _)| id.clone())
        .collect()
}

/// The entry a workspace-level request naming `workspace` should read its shape from: the caller's
/// own context when they are standing in one of the workspace's, else any loaded context of it.
///
/// Requests name a *workspace*, because that is what the client knows — `WorkspaceInfo::name` is the
/// name, never the internal context id. When the caller is in a bound context, the name is not a key
/// in the map at all, so it has to be resolved rather than used directly.
pub fn request_context(
    s: &crate::state::ServerState,
    client_id: ClientId,
    workspace: &str,
) -> Option<String> {
    // The caller's own context first. An unbound context's id *is* its name, so checking the map
    // for the name would otherwise short-circuit to the **base** whenever it happens to be loaded
    // — and a client standing in a worktree would silently edit and be shown the base's shape.
    let active = s
        .clients
        .get(&client_id)
        .and_then(|c| c.active_workspace.clone())
        .filter(|id| {
            s.workspaces
                .get(id)
                .is_some_and(|e| e.name.as_deref() == Some(workspace))
        });
    active
        .or_else(|| {
            s.workspaces
                .contains_key(workspace)
                .then(|| workspace.to_string())
        })
        .or_else(|| contexts_of(s, workspace).into_iter().next())
}

/// The bindings a **loaded** context is resolved against, read off its entry.
///
/// The entry is the source of truth once loaded: the session file records contexts for restoring
/// them, but a context that is open has already resolved which one it is, and re-reading the file
/// would be a second copy to keep in step.
pub fn loaded_bindings(
    s: &crate::state::ServerState,
    context: &str,
) -> std::collections::BTreeMap<std::path::PathBuf, String> {
    s.workspaces
        .get(context)
        .map(|e| e.worktrees.clone())
        .unwrap_or_default()
}

/// Normalise a client-sent binding map — keyed by [`RepoId`], i.e. a workdir — onto repo
/// **families**, keyed by common dir.
///
/// One place, at the wire boundary. Clients speak the ids picker rows carry, which name a checkout;
/// bindings are per family, so a set sent from inside a worktree has to land on the same key as one
/// sent from the main checkout or it would write a second entry for the same repo. Doing it here
/// means nothing downstream has to remember to.
///
/// Entries naming a path that is no repo are dropped rather than refused: a binding that cannot
/// resolve is machine state that no longer resolves, which is discarded everywhere else too.
pub fn normalise_bindings(
    wire: &std::collections::BTreeMap<aether_protocol::git::RepoId, String>,
) -> std::collections::BTreeMap<std::path::PathBuf, String> {
    wire.iter()
        .filter_map(|(repo_id, admin)| {
            crate::git::discover_repo(Path::new(repo_id))
                .map(|identity| (identity.common_dir, admin.clone()))
        })
        .collect()
}

/// The wire form of a context's bindings, for [`WorkspaceInfo`]: family keys mapped back to the
/// **main** checkout's `RepoId`, with each tree's current branch alongside.
///
/// The branch is read here rather than derived from the admin name, because the two drift — a tree
/// made for `feature/auth` can be sitting on `main` — and a label that guessed would be wrong
/// exactly when it mattered.
pub fn wire_bindings(
    bindings: &std::collections::BTreeMap<std::path::PathBuf, String>,
) -> Vec<aether_protocol::workspace::WorkspaceWorktree> {
    bindings
        .iter()
        .filter_map(|(common_dir, admin)| {
            // The main checkout is the common dir's parent — the same recovery `worktree::list`
            // makes, with the same known gap for bare and `--separate-git-dir` repos, where it
            // simply yields nothing rather than something wrong.
            let main = common_dir.parent()?;
            let branch = crate::worktree::path_for_name(main, admin)
                .and_then(|path| git2::Repository::open(path).ok())
                .and_then(|repo| crate::git::head_state(&repo))
                .and_then(|head| match head {
                    aether_protocol::git::GitHead::Branch { name, .. }
                    | aether_protocol::git::GitHead::Unborn { name } => Some(name),
                    aether_protocol::git::GitHead::Detached { .. } => None,
                })
                .unwrap_or_default();
            Some(aether_protocol::workspace::WorkspaceWorktree {
                repo_id: path_string(main),
                worktree: admin.clone(),
                branch,
            })
        })
        .collect()
}

/// The row naming where the caller already is, as a picker's initial highlight on a fresh open:
/// the checkout you are standing in for the branch picker, the baseline in force for the baseline
/// picker.
///
/// "Where you are" is the selection, not a glyph on the row — the same move Views and Workspaces
/// make. Both of these pickers used to mark the row with a `●` in a reserved leading column;
/// expressing it by opening *on* the row is what let those markers go.
///
/// Only on a fresh open: a re-view (scroll, resume) must keep whatever the user has highlighted.
pub fn current_state_item(
    picker: &picker_state::PickerState,
    reset: PickerReset,
) -> Option<PickerItem> {
    if reset != PickerReset::All {
        return None;
    }
    let idx = match &picker.candidates {
        // The current *checkout* rather than `is_head`: standing in a detached worktree there is
        // no head branch to find, and its row — keyed by admin name — is the one to land on. The
        // two coincide for every branch row, since `list_branches` records the tree you are in
        // like any other.
        picker_state::PickerCandidates::GitBranches(v) => v
            .iter()
            .position(|c| c.row.checkout.as_ref().is_some_and(|k| k.is_current))
            .or_else(|| v.iter().position(|c| c.row.is_head)),
        // The baseline in force, for the same reason and by the same means. `(index)` is the
        // default, so an unpinned repo opens on the first row — which is where it would have
        // opened anyway, and is now saying something.
        picker_state::PickerCandidates::GitBaseline(v) => v.iter().position(|c| c.current),
        _ => None,
    }?;
    Some(picker.candidates.make_item(idx, Vec::new()))
}

/// Create a worktree — see [`aether_protocol::git::GitWorktreeAdd`].
///
/// Shape mirrors push and pull: everything decidable without spawning git is decided first (all
/// three refusals below are permanent, so a round trip could only confirm them), then the checkout
/// runs streamed and cancellable, then the result is classified from *our* reads rather than from
/// git's wording.
///
/// The one thing that is neither: the family lock. `git worktree add` rewrites `.git/config` in the
/// common dir under git's own lockfile, so two concurrent adds in one family leave one dead with
/// `could not lock config file`. Waiting is right where refusing would be an error the user has to
/// understand — two agents asking at once is the expected case.
pub async fn git_worktree_add(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: GitWorktreeAddParams,
) -> Result<GitWorktreeAddResult, RpcError> {
    let (workdir, common_dir, store_override) = {
        let s = state.lock().await;
        let repo =
            resolve_writable_repo(&s, ctx.client_id, params.repo_id.as_ref(), params.buffer_id)?;
        (
            std::path::PathBuf::from(&repo.repo_id),
            std::path::PathBuf::from(&repo.common_dir),
            s.worktree_store.clone(),
        )
    };
    let branch = params.branch.trim().to_string();
    if branch.is_empty() {
        return Ok(GitWorktreeAddResult {
            status: GitWorktreeAddStatus::InvalidBranchName,
            ..Default::default()
        });
    }

    let refuse = |status| {
        Ok(GitWorktreeAddResult {
            status,
            ..Default::default()
        })
    };

    // Serialise on the family *before* the preflight, not after. The preflight's questions — does
    // this branch exist, is it checked out somewhere already — are exactly the ones a concurrent
    // add in the same family is about to change the answers to. Asked outside the lock, two agents
    // creating the same branch both passed, and the one that lost the race got `Refused` with git's
    // raw stderr instead of `AlreadyCheckedOut` and somewhere to go. Two agents at once is the case
    // this lock exists for, so it is the case worth classifying well.
    let lock = state.lock().await.worktree_lock(&common_dir);
    let _guard = lock.lock().await;

    let pre = {
        let workdir = workdir.clone();
        let branch = branch.clone();
        let create = params.create_branch;
        tokio::task::spawn_blocking(move || worktree_add_preflight(&workdir, &branch, create))
            .await
            .map_err(|e| RpcError::internal(format!("reading repo state: {e}")))?
    };
    match pre.status {
        GitWorktreeAddStatus::Created => {}
        GitWorktreeAddStatus::AlreadyCheckedOut => {
            return Ok(GitWorktreeAddResult {
                status: GitWorktreeAddStatus::AlreadyCheckedOut,
                checked_out_in: pre.checked_out_in,
                ..Default::default()
            })
        }
        other => return refuse(other),
    }

    // Settings read, directory creation and the worktree listing are all blocking — the listing
    // opens a `git2::Repository` per tree to read its HEAD — so they go to the pool like every
    // other libgit2 call in this handler, rather than stalling the runtime while the family lock
    // is held.
    let (store_dir, admin) = {
        let common_dir = common_dir.clone();
        let workdir = workdir.clone();
        let branch = branch.clone();
        let store_override = store_override.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<(std::path::PathBuf, String)> {
            let store_dir = crate::worktree::store_root(store_override.as_deref())?
                .join(crate::worktree::repo_key(&common_dir));
            std::fs::create_dir_all(&store_dir)?;
            let existing: Vec<String> = crate::worktree::list(&workdir)
                .into_iter()
                .map(|row| row.name)
                .collect();
            let admin = crate::worktree::unique_admin_name(
                &existing,
                &store_dir,
                &crate::worktree::admin_name_for_branch(&branch),
            );
            Ok((store_dir, admin))
        })
        .await
        .map_err(|e| RpcError::internal(format!("preparing worktree store: {e}")))?
        .map_err(|e| RpcError::internal(format!("preparing worktree store: {e}")))?
    };
    let path = store_dir.join(&admin);
    let path_str = path.to_string_lossy().into_owned();

    // `git worktree add` derives its admin id from the *directory basename*, which is exactly the
    // name we just uniquified — so git's own id and ours agree without passing one explicitly.
    let args: Vec<&str> = if params.create_branch {
        vec!["worktree", "add", "-b", &branch, &path_str]
    } else {
        vec!["worktree", "add", &path_str, &branch]
    };
    let (output, cancelled) =
        match run_network_git(state, &workdir, &args, Some(GitOperationKind::WorktreeAdd)).await {
            Ok(v) => v,
            Err(e) => return Err(RpcError::internal(format!("running git worktree add: {e}"))),
        };

    if cancelled || !output.success() {
        // A killed or failed `worktree add` can leave a half-populated directory *and* an admin
        // entry. Clean both up here, while we still know the tree is ours and was created seconds
        // ago — this is the one context in which removing a worktree needs no confirmation.
        cleanup_failed_worktree(&workdir, &path).await;
        if cancelled {
            return refuse(GitWorktreeAddStatus::Cancelled);
        }
        let message = if output.stderr.trim().is_empty() {
            output.stdout
        } else {
            output.stderr
        };
        return Ok(GitWorktreeAddResult {
            status: GitWorktreeAddStatus::Refused,
            message: message.trim_end().to_string(),
            ..Default::default()
        });
    }

    // Seeding runs under its own operation, so the status bar keeps saying "Creating worktree" and
    // `Space g x` keeps working through it. Copying a `node_modules` is the slowest part of the
    // whole command, and it used to run *after* the checkout's operation was deregistered — no
    // indicator, no way to stop it, just an editor that looked wedged for a minute.
    let (seed_token, seed_reporter) =
        begin_operation(state, &workdir, GitOperationKind::WorktreeAdd).await;
    let (seeded_files, has_submodules, row) = {
        let source = workdir.clone();
        let dest = path.clone();
        let admin = admin.clone();
        let token = seed_token.clone();
        tokio::task::spawn_blocking(move || {
            let seeded =
                crate::worktree::seed_from_include_file(&source, &dest, &|| *token.borrow());
            let submodules = crate::worktree::has_submodules(&source);
            let row = crate::worktree::list(&source)
                .into_iter()
                .find(|r| r.name == admin);
            (seeded, submodules, row)
        })
        .await
        .map_err(|e| RpcError::internal(format!("finishing worktree: {e}")))?
    };
    seed_reporter.finish(state).await;
    // A stopped *seed* is not a stopped create: the worktree exists and is a perfectly good
    // checkout, it just has fewer gitignored files in it than asked for. Reported as created, with
    // the count telling the truth about what landed.
    if *seed_token.borrow() {
        tracing::info!(worktree = %admin, seeded_files, "worktree seeding stopped early");
    }

    // Correct the open picker underneath the user. Creating deliberately leaves it up and does not
    // move you (`Ctrl-o` is a lifecycle verb, not a navigation one), so the row gaining its worktree
    // marker *is* the feedback that it worked — without this the list still says the branch has no
    // tree, and the next press would try to make a second one. Removal has needed this since it
    // stopped closing the picker; creation needs it for the same reason, which only became true
    // once the two verbs were split.
    refresh_worktree_pickers(state).await;
    tracing::info!(worktree = %admin, branch = %branch, seeded_files, "worktree created");
    Ok(GitWorktreeAddResult {
        status: GitWorktreeAddStatus::Created,
        worktree: row.or(Some(GitWorktreeRow {
            name: admin,
            path: path_str,
            ..Default::default()
        })),
        seeded_files,
        has_submodules,
        ..Default::default()
    })
}

/// Everything about a `worktree add` that can be answered without spawning git.
struct WorktreeAddPreflight {
    status: GitWorktreeAddStatus,
    checked_out_in: Option<String>,
}

fn worktree_add_preflight(workdir: &Path, branch: &str, create: bool) -> WorktreeAddPreflight {
    let ok = |status| WorktreeAddPreflight {
        status,
        checked_out_in: None,
    };
    let Ok(repo) = git2::Repository::open(workdir) else {
        return ok(GitWorktreeAddStatus::Refused);
    };
    // No commit means no tree to check out. Git's own complaint here names `HEAD` and reads like an
    // internal error, so this is worth answering ourselves.
    if matches!(crate::git::head_state(&repo), Some(GitHead::Unborn { .. })) {
        return ok(GitWorktreeAddStatus::Unborn);
    }
    let exists = repo.find_branch(branch, git2::BranchType::Local).is_ok();
    if create {
        if !git2::Reference::is_valid_name(&format!("refs/heads/{branch}")) {
            return ok(GitWorktreeAddStatus::InvalidBranchName);
        }
        // An existing branch with `create` set falls through to git, whose "already exists" is
        // clearer than anything we'd invent.
        return ok(GitWorktreeAddStatus::Created);
    }
    if !exists {
        return ok(GitWorktreeAddStatus::NoSuchBranch);
    }
    // Git allows one checkout of a branch across the whole family. The picker normally keeps this
    // off the list entirely, so reaching it means the list went stale — the client's move
    // is still to *go there* rather than report a failure.
    if let Some(other) = crate::git::branch_checked_out_elsewhere(workdir, branch) {
        return WorktreeAddPreflight {
            status: GitWorktreeAddStatus::AlreadyCheckedOut,
            checked_out_in: Some(other),
        };
    }
    ok(GitWorktreeAddStatus::Created)
}

/// Undo a `worktree add` that was cancelled or failed part-way.
///
/// `git worktree remove --force` first, because it takes the admin entry with it *and* only this
/// entry. Only if that fails do we delete the directory and prune — the ordering matters, because
/// `prune` is family-wide (git has no way to scope it to one entry) and so can also take any other
/// tree of this family whose directory happens to be missing. Reaching it means `remove --force`
/// already failed on a tree we created seconds ago, which is rare enough to accept that; see the
/// prunable branch of [`git_worktree_remove`] for the same trade stated in full.
async fn cleanup_failed_worktree(workdir: &Path, path: &Path) {
    let path_str = path.to_string_lossy().into_owned();
    let removed = crate::git_cli::run(workdir, &["worktree", "remove", "--force", &path_str])
        .await
        .is_ok_and(|o| o.success());
    if removed {
        return;
    }
    if path.exists() {
        let _ = std::fs::remove_dir_all(path);
    }
    let _ = crate::git_cli::run(workdir, &["worktree", "prune"]).await;
}

/// Remove a worktree — see [`aether_protocol::git::GitWorktreeRemove`].
///
/// The guard is git's own refusal, not a reimplementation of it; what we add is the *itemisation*,
/// so the client can say "2 modified, 1 untracked" instead of "are you sure?". Removing a worktree
/// never touches its branch, so committed work is never in that list.
pub async fn git_worktree_remove(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: GitWorktreeRemoveParams,
) -> Result<GitWorktreeRemoveResult, RpcError> {
    let (workdir, common_dir) = {
        let s = state.lock().await;
        let repo =
            resolve_writable_repo(&s, ctx.client_id, params.repo_id.as_ref(), params.buffer_id)?;
        (
            std::path::PathBuf::from(&repo.repo_id),
            std::path::PathBuf::from(&repo.common_dir),
        )
    };
    let refuse = |status| {
        Ok(GitWorktreeRemoveResult {
            status,
            ..Default::default()
        })
    };

    let rows = {
        let workdir = workdir.clone();
        tokio::task::spawn_blocking(move || crate::worktree::list(&workdir))
            .await
            .map_err(|e| RpcError::internal(format!("listing worktrees: {e}")))?
    };
    let Some(target) = rows.iter().find(|r| r.name == params.name) else {
        // The main worktree carries no admin name, so an empty name can only mean it.
        if params.name.is_empty() {
            return refuse(GitWorktreeRemoveStatus::IsMain);
        }
        return refuse(GitWorktreeRemoveStatus::NotFound);
    };
    if target.is_main {
        return refuse(GitWorktreeRemoveStatus::IsMain);
    }
    // Locking is a deliberate act — a tree on removable media, a long-running experiment — so it is
    // never escalated past automatically, not even by `force`.
    if target.locked {
        return refuse(GitWorktreeRemoveStatus::Locked);
    }
    let target_path = std::path::PathBuf::from(&target.path);

    // Aether's own unsaved work, which git cannot see: a buffer open on a file in this tree with
    // edits that were never written. `--force` would take the file out from under it. Refused
    // rather than reported, unlike the root remap — this really does destroy the context.
    {
        let s = state.lock().await;
        let dirty: Vec<BufferId> = s
            .documents
            .iter()
            .filter(|(_, d)| d.dirty)
            .filter_map(|(doc_id, d)| {
                let path = d.canonical_path.as_deref()?;
                path.starts_with(&target_path).then_some(*doc_id)
            })
            .flat_map(|doc_id| {
                s.buffers
                    .iter()
                    .filter(move |(_, b)| b.document == doc_id)
                    .map(|(id, _)| *id)
            })
            .collect();
        if !dirty.is_empty() {
            let mut err = RpcError::new(
                ErrorCode::DIRTY_BUFFERS_PREVENT_REMOVE,
                format!(
                    "{} buffer(s) in {} have unsaved changes",
                    dirty.len(),
                    target_path.display()
                ),
            );
            err.data = Some(serde_json::json!({ "dirty_buffer_ids": dirty }));
            return Err(err);
        }
    }

    // A prunable row's directory is already gone, so `git worktree remove` has nothing to remove
    // and would refuse. Pruning is right *here* and nowhere else: we have just watched `validate`
    // fail for this specific entry, which is the condition that must hold before running a
    // command that has no grace period.
    //
    // It is worth being honest that `prune` is **family-wide** — git gives no way to prune one
    // entry — so any *other* worktree of this family whose directory is currently missing loses its
    // admin entry too. Locked trees are skipped by git itself, which covers the deliberate case
    // (removable media); an unlocked tree on an unmounted disk is the real exposure. Bounded rather
    // than eliminated: a binding onto a pruned tree degrades to the configured root at the next
    // materialisation rather than failing, so the cost is a rebind, never lost work.
    if target.prunable {
        release_worktree_bindings(state, ctx.client_id, &target.name).await?;
        let cwd = main_worktree_dir(&rows).unwrap_or_else(|| workdir.clone());
        let lock = state.lock().await.worktree_lock(&common_dir);
        let _guard = lock.lock().await;
        let out = crate::git_cli::run(&cwd, &["worktree", "prune"])
            .await
            .map_err(|e| RpcError::internal(format!("running git worktree prune: {e}")))?;
        drop(_guard);
        refresh_worktree_pickers(state).await;
        return if out.success() {
            refuse(GitWorktreeRemoveStatus::Removed)
        } else {
            Ok(GitWorktreeRemoveResult {
                status: GitWorktreeRemoveStatus::Refused,
                message: out.stderr.trim_end().to_string(),
                ..Default::default()
            })
        };
    }

    // No `!target.prunable` here: the prunable branch above returned.
    if !params.force {
        let at_risk = {
            let path = target_path.clone();
            tokio::task::spawn_blocking(move || crate::worktree::at_risk(&path))
                .await
                .map_err(|e| RpcError::internal(format!("reading worktree status: {e}")))?
        };
        if at_risk.modified > 0 || at_risk.untracked > 0 || at_risk.operation_in_progress {
            return Ok(GitWorktreeRemoveResult {
                status: GitWorktreeRemoveStatus::Dirty,
                at_risk: Some(at_risk),
                ..Default::default()
            });
        }
    }

    // Take every workspace off this tree before it goes, so nothing is left pointing into the hole
    // (see `release_worktree_bindings`). After the dirty-buffer guard above, so unsaved work still
    // refuses the whole operation rather than being moved first and refused second.
    release_worktree_bindings(state, ctx.client_id, &params.name).await?;

    let lock = state.lock().await.worktree_lock(&common_dir);
    let _guard = lock.lock().await;

    // Run from the *main* worktree, never from inside the tree being removed: a command whose cwd
    // vanishes underneath it is a class of failure worth not having.
    let cwd = main_worktree_dir(&rows).unwrap_or_else(|| workdir.clone());
    let path_str = target.path.clone();
    let mut args = vec!["worktree", "remove"];
    if params.force {
        args.push("--force");
    }
    args.push(&path_str);
    let output = crate::git_cli::run(&cwd, &args)
        .await
        .map_err(|e| RpcError::internal(format!("running git worktree remove: {e}")))?;

    if !output.success() {
        let message = if output.stderr.trim().is_empty() {
            output.stdout
        } else {
            output.stderr
        };
        return Ok(GitWorktreeRemoveResult {
            status: GitWorktreeRemoveStatus::Refused,
            message: message.trim_end().to_string(),
            ..Default::default()
        });
    }
    // Bindings held by workspaces that aren't loaded were not touched above (there was nothing to
    // re-materialise), so drop those now — a binding naming a tree that no longer exists would open
    // the workspace onto nothing the next time it is activated.
    forget_worktree_bindings(state, &params.name).await?;
    refresh_worktree_pickers(state).await;
    tracing::info!(worktree = %params.name, "worktree removed");
    refuse(GitWorktreeRemoveStatus::Removed)
}

/// Push a corrected row set to every open worktree picker.
///
/// Removal is the only worktree row action that leaves the picker **up**: creating or binding both
/// end in a switch that closes it, and there is no confirm dialog here to close it either (the
/// refusal is the confirmation — see the client's chord). So the list has to be corrected
/// underneath the user, or the tree they just removed stays on screen and the next press acts on a
/// row that no longer exists.
///
/// By push rather than a client-side re-open, for the reason the stash and branch pickers already
/// take this route: the list is repo-wide, and a push keeps the query and the highlight that a
/// fresh open would wipe.
async fn refresh_worktree_pickers(state: &SharedState) {
    let pushes = {
        let mut s = state.lock().await;
        refresh_git_ref_pickers(&mut s, PickerKind::GitBranches)
    };
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
}

/// The main worktree's directory, from a listing. `None` for a family whose main tree wasn't
/// recoverable (a bare or `--separate-git-dir` repo), where the caller falls back to its own.
fn main_worktree_dir(rows: &[GitWorktreeRow]) -> Option<std::path::PathBuf> {
    rows.iter()
        .find(|r| r.is_main)
        .map(|r| std::path::PathBuf::from(&r.path))
}

/// Take every **loaded** workspace off `worktree` before the tree is deleted: drop the bindings and
/// re-materialise the roots, so the workspaces are back on their configured paths and their buffers
/// are reopened there.
///
/// Without this, removing the worktree a workspace is standing in leaves that workspace pointing at
/// a directory that no longer exists — roots, workspace index, watches and open buffers all aimed
/// at deleted paths, with only a server restart to recover, since nothing re-materialises a
/// workspace whose bindings changed underneath it. Unbinding first is the fix, done for the user
/// rather than demanded of them.
///
/// Runs **before** the removal, not after: the point is that no buffer is left open on a file that
/// is about to be deleted. The cost is that a removal which then fails leaves you unbound — visible,
/// recoverable in one keystroke, and far better than the reverse (a removal that succeeds while a
/// workspace keeps pointing into the hole it left).
async fn release_worktree_bindings(
    state: &SharedState,
    client_id: ClientId,
    worktree: &str,
) -> Result<(), RpcError> {
    // Every *loaded context* bound to this tree, read off the entries themselves — the session file
    // records contexts for restoring them, but a context that is open has already resolved which
    // one it is.
    let bound: Vec<String> = {
        let s = state.lock().await;
        s.workspaces
            .iter()
            .filter(|(_, e)| e.worktrees.values().any(|name| name == worktree))
            .map(|(id, _)| id.clone())
            .collect()
    };
    // Unconditionally, and *not* gated on `bound` being non-empty: a workspace that isn't currently
    // loaded holds its binding in the session file just the same, and leaving it there aims it at a
    // worktree we are about to delete. `bound` decides only which workspaces additionally need
    // re-materialising in memory, which is a question about this process, not about the file.
    forget_worktree_bindings(state, worktree).await?;
    for id in bound {
        rebind_loaded_workspace(state, client_id, &id, None).await?;
    }
    Ok(())
}

/// Drop every binding naming `worktree`, from every workspace that holds one.
///
/// The workspaces themselves survive — losing a binding just puts those roots back on the
/// configured path, which is the same thing unbinding does. Runs after a successful removal, and
/// after a prune, which are the two moments a worktree stops existing by our own hand.
///
/// Doing it here rather than leaving the next activation to notice keeps the roots honest: a
/// workspace pointing into a directory we just deleted would open onto nothing.
async fn forget_worktree_bindings(state: &SharedState, worktree: &str) -> Result<(), RpcError> {
    let Some(path) = state.lock().await.sessions_path.clone() else {
        return Ok(());
    };
    let mut sessions = crate::config::load_workspace_sessions_at(&path).unwrap_or_default();
    let mut changed = false;
    for entry in sessions.workspaces.values_mut() {
        let before = entry.contexts.len();
        for ctx in entry.contexts.iter_mut() {
            let held = ctx.worktrees.len();
            ctx.worktrees.retain(|_, name| name != worktree);
            changed |= ctx.worktrees.len() != held;
        }
        // A context whose last binding just went is the base, and the base already has an entry —
        // this one. Leaving it would be a second, empty context shadowing it.
        entry.contexts.retain(|c| !c.worktrees.is_empty());
        changed |= entry.contexts.len() != before;
    }
    if !changed {
        return Ok(());
    }
    crate::config::write_workspace_sessions_at(&path, &sessions)
        .map_err(|e| RpcError::internal(format!("updating worktree bindings: {e}")))
}

/// Reconcile every open buffer in a repo with the working tree after it moved wholesale.
///
/// Scoped to the *repo*, not the workspace: a document open in two workspaces moved for both, so
/// this walks the Git baselines (which is also where a buffer's repo is already resolved) rather
/// than one workspace's buffer list.
///
/// The per-buffer policy is the file watcher's, applied deliberately instead of incidentally:
/// clean buffers re-read, dirty buffers flagged rather than clobbered, vanished files flagged
/// rather than closed. Every buffer's Git baseline is recomputed regardless of whether its file
/// changed — a commit moves HEAD without touching a single working-tree file, and the gutter has
/// to follow.
pub async fn git_refresh(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: GitRefreshParams,
) -> Result<GitRefreshResult, RpcError> {
    let mut s = state.lock().await;
    let repo = resolve_repo(&s, ctx.client_id, &params.repo_id)?;
    let workdir = std::path::PathBuf::from(&repo.repo_id);
    let (result, pushes) = reconcile_repo(&mut s, &workdir);
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    reconcile_working_changes(state, &workdir).await;
    Ok(result)
}

/// Open buffers in this repo that hold unsaved edits — the checkout pre-flight.
///
/// Same enumeration as [`reconcile_repo`]'s (the Git baselines, which is where a buffer's repo is
/// already resolved), asked *before* the operation instead of after. Git cannot see these: it
/// guards files on disk, so it would rewrite a file underneath an unsaved buffer and leave the
/// user's edits sitting on the wrong base.
fn dirty_buffers_in_repo(s: &ServerState, workdir: &std::path::Path) -> Vec<BufferId> {
    let mut ids: Vec<BufferId> = s
        .git_baseline
        .iter()
        .filter(|(_, b)| b.repo.as_ref().is_some_and(|r| r.workdir == workdir))
        .map(|(id, _)| *id)
        .filter(|id| s.try_doc_of(*id).is_some_and(|d| d.dirty))
        .collect();
    ids.sort_unstable(); // `git_baseline` is a HashMap; keep the reported list stable
    ids
}

/// Switch branches, creating the branch first when asked.
///
/// Structurally the same as [`git_commit`] — resolve, suppress, spawn, reconcile — with one thing
/// commit doesn't need in front: the dirty-buffer pre-flight. Commit doesn't rewrite the working
/// tree; checkout does, and unsaved buffers are invisible to git.
pub async fn git_checkout(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: GitCheckoutParams,
) -> Result<GitCheckoutResult, RpcError> {
    let (workdir, blocked) = {
        let s = state.lock().await;
        let repo =
            resolve_writable_repo(&s, ctx.client_id, params.repo_id.as_ref(), params.buffer_id)?;
        let workdir = std::path::PathBuf::from(&repo.repo_id);
        let blocked = dirty_buffers_in_repo(&s, &workdir);
        (workdir, blocked)
    };

    // Refuse before anything happens, and say what to do about it. Deliberately not "save them for
    // the user": saving is a user act, and a checkout is a bad moment to perform one silently.
    if !blocked.is_empty() {
        return Ok(GitCheckoutResult {
            status: GitCheckoutStatus::BlockedByDirtyBuffers,
            blocked,
            ..Default::default()
        });
    }

    // A branch can be checked out in one worktree at a time. Answer that from libgit2 rather than
    // letting git refuse, so the message can name the worktree. Irrelevant when creating — a brand
    // new branch is checked out nowhere.
    if !params.create {
        let held = tokio::task::spawn_blocking({
            let workdir = workdir.clone();
            let branch = params.branch.clone();
            move || crate::git::branch_checked_out_elsewhere(&workdir, &branch)
        })
        .await
        .unwrap_or_default();
        if let Some(worktree) = held {
            return Ok(GitCheckoutResult {
                status: GitCheckoutStatus::AlreadyCheckedOut,
                message: worktree,
                ..Default::default()
            });
        }
    }

    // A checkout rewrites many files at once, so it runs through the tree-op helper: suppression
    // around the spawn (without it the watcher races the reconciliation and the report — the only
    // thing that tells the user what moved — comes back empty) and a reconcile either way.
    let mut args: Vec<&str> = vec!["checkout"];
    if params.create {
        args.push("-b");
    }
    args.push(&params.branch);
    let run = run_tree_git(state, &workdir, &args, None, None).await?;
    let refreshed = run.refreshed;

    if !run.output.success() {
        // git narrates checkout on stderr ("Switched to branch 'x'" included), so a failure's
        // useful text is there; fall back to stdout the way `git_commit` does.
        return Ok(GitCheckoutResult {
            status: GitCheckoutStatus::Refused,
            message: git_failure_message(&run.output),
            refreshed,
            ..Default::default()
        });
    }

    let head = tokio::task::spawn_blocking({
        let workdir = workdir.clone();
        move || crate::git::discover_repo(&workdir).map(|i| i.head)
    })
    .await
    .ok()
    .flatten();

    Ok(GitCheckoutResult {
        status: if params.create {
            GitCheckoutStatus::Created
        } else {
            GitCheckoutStatus::Switched
        },
        head,
        blocked: Vec::new(),
        message: String::new(),
        refreshed,
    })
}

/// What a tree-rewriting git command produced — see [`run_tree_git`].
struct TreeRun {
    output: crate::git_cli::GitOutput,
    /// Read off the cancel token, never out of the exit status: a killed git and a crashed one look
    /// identical from the outside.
    cancelled: bool,
    refreshed: GitRefreshResult,
}

/// Run a git command that rewrites the working tree, with the full checkout ceremony: watcher
/// suppression, reconciliation either way, and — for the network ones — an announced, cancellable
/// invocation.
///
/// Every tree-moving operation shares these hazards and none of them may skip one, which is the
/// whole argument for a single helper: duplicating the sequence is how one of them ends up without
/// a reconcile. What callers keep for themselves is the *dirty-buffer pre-flight* (each maps it to
/// its own refusal status, and checkout has a second pre-flight to interleave with it) and the
/// classification of the result.
///
/// `announce` makes this the network path: `Some` streams progress and honours [`GitCancel`],
/// `None` is the plain local run stash and checkout want.
///
/// **Suppression is held across the network transfer too, and unlike [`fetch_repo`] that is
/// correct.** The objection there — suppression is keyed by workdir containment, so holding it for
/// the length of a slow transfer blinds the watcher to the user's own edits — does not apply to an
/// operation that ends in [`reconcile_repo`], which re-stats every buffer in the repo and refreshes
/// the explorer and view pickers. That pass is a superset of what the suppressed watcher would
/// have done, so nothing is lost, only deferred to the end. A fetch has no such pass, which is
/// exactly why it must not suppress.
async fn run_tree_git(
    state: &SharedState,
    workdir: &Path,
    args: &[&str],
    announce: Option<GitOperationKind>,
    refresh_picker: Option<PickerKind>,
) -> Result<TreeRun, RpcError> {
    state
        .lock()
        .await
        .git_suppressed
        .insert(workdir.to_path_buf());
    let outcome = run_network_git(state, workdir, args, announce).await;

    let mut s = state.lock().await;
    s.git_suppressed.remove(workdir);
    let (output, cancelled) = match outcome {
        Ok(v) => v,
        Err(e) => {
            drop(s);
            let verb = args.first().copied().unwrap_or("command");
            return Err(RpcError::internal(format!("running git {verb}: {e}")));
        }
    };
    // Reconcile either way: a command that failed partway can still have touched files, and a
    // buffer left stale is worse than a redundant pass.
    let (refreshed, mut pushes) = reconcile_repo(&mut s, workdir);
    if let Some(kind) = refresh_picker {
        pushes.extend(refresh_git_ref_pickers(&mut s, kind));
    }
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    reconcile_working_changes(state, workdir).await;

    Ok(TreeRun {
        output,
        cancelled,
        refreshed,
    })
}

/// git's useful text from a failed run: its stderr, falling back to stdout for the commands that
/// narrate there. Trimmed, because this goes straight into a toast.
fn git_failure_message(output: &crate::git_cli::GitOutput) -> String {
    let message = if output.stderr.trim().is_empty() {
        &output.stdout
    } else {
        &output.stderr
    };
    message.trim_end().to_string()
}

/// Run a tree-rewriting stash command (`push` / `apply` / `pop`). The three differ only in argv and
/// the status they report on success.
async fn run_tree_stash(
    state: &SharedState,
    workdir: std::path::PathBuf,
    args: Vec<String>,
    ok_status: GitStashStatus,
) -> Result<GitStashResult, RpcError> {
    let blocked = {
        let s = state.lock().await;
        dirty_buffers_in_repo(&s, &workdir)
    };
    // Refuse before anything runs. A stash rewrites the working tree under any unsaved buffer, and
    // saving on the user's behalf is not ours to do — the same call checkout makes.
    if !blocked.is_empty() {
        return Ok(GitStashResult {
            status: GitStashStatus::BlockedByDirtyBuffers,
            blocked,
            ..Default::default()
        });
    }

    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    // The entry list changed under any open stash picker — including after a *failed* run, which
    // may still have created or consumed an entry.
    let run = run_tree_git(state, &workdir, &argv, None, Some(PickerKind::GitStash)).await?;

    if !run.output.success() {
        return Ok(GitStashResult {
            status: GitStashStatus::Refused,
            message: git_failure_message(&run.output),
            refreshed: run.refreshed,
            ..Default::default()
        });
    }
    Ok(GitStashResult {
        status: ok_status,
        refreshed: run.refreshed,
        message: run.output.stdout.trim_end().to_string(),
        ..Default::default()
    })
}

/// `git stash push`: shelve the working tree.
pub async fn git_stash_push(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: GitStashPushParams,
) -> Result<GitStashResult, RpcError> {
    let workdir = {
        let s = state.lock().await;
        let repo =
            resolve_writable_repo(&s, ctx.client_id, params.repo_id.as_ref(), params.buffer_id)?;
        std::path::PathBuf::from(&repo.repo_id)
    };
    // `--staged` arrived in git 2.35. Asked before running, because an older git answers an
    // unknown option with its usage text — a wall of output that says nothing about what the user
    // pressed. Only probed when the flag is actually wanted: it costs a child process.
    if params.staged && !crate::git_cli::at_least(&workdir, 2, 35).await {
        return Ok(GitStashResult {
            status: GitStashStatus::StagedUnsupported,
            ..Default::default()
        });
    }
    // "Nothing to stash" is a success exit for git, so ask *before* running rather than parsing
    // its narration afterwards: reporting "stashed" when nothing was is a lie the user would act
    // on (they'd go looking for an entry that doesn't exist). `--staged` asks the narrower
    // question — a tree with only unstaged work is not clean, but it holds nothing this stash
    // would take.
    let nothing_to_stash = {
        let workdir = workdir.clone();
        let staged = params.staged;
        tokio::task::spawn_blocking(move || {
            if staged {
                !crate::git::has_staged_changes(&workdir)
            } else {
                crate::git::changed_files_in_repo(&workdir).is_empty()
            }
        })
        .await
        .unwrap_or(false)
    };
    if nothing_to_stash {
        return Ok(GitStashResult {
            status: GitStashStatus::NothingToStash,
            ..Default::default()
        });
    }

    let mut args = vec!["stash".to_string(), "push".to_string()];
    if params.staged {
        args.push("--staged".to_string());
    }
    if let Some(message) = params.message.as_deref().filter(|m| !m.trim().is_empty()) {
        args.push("-m".to_string());
        args.push(message.to_string());
    }
    run_tree_stash(state, workdir, args, GitStashStatus::Pushed).await
}

/// `git stash apply` / `pop`: restore an entry into the working tree.
pub async fn git_stash_apply(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: GitStashApplyParams,
) -> Result<GitStashResult, RpcError> {
    let workdir = {
        let s = state.lock().await;
        let repo =
            resolve_writable_repo(&s, ctx.client_id, params.repo_id.as_ref(), params.buffer_id)?;
        std::path::PathBuf::from(&repo.repo_id)
    };
    let Some(spec) = stash_spec(&workdir, &params.oid).await else {
        return Ok(GitStashResult {
            status: GitStashStatus::Gone,
            ..Default::default()
        });
    };
    let verb = if params.pop { "pop" } else { "apply" };
    let args = vec!["stash".to_string(), verb.to_string(), spec];
    let ok = if params.pop {
        GitStashStatus::Popped
    } else {
        GitStashStatus::Applied
    };
    run_tree_stash(state, workdir, args, ok).await
}

/// `git stash drop`: discard an entry. No tree move, so no pre-flight and nothing to reconcile —
/// the split `git/delete_branch` has from `git/checkout`.
pub async fn git_stash_drop(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: GitStashDropParams,
) -> Result<GitStashResult, RpcError> {
    let workdir = {
        let s = state.lock().await;
        let repo =
            resolve_writable_repo(&s, ctx.client_id, params.repo_id.as_ref(), params.buffer_id)?;
        std::path::PathBuf::from(&repo.repo_id)
    };
    let Some(spec) = stash_spec(&workdir, &params.oid).await else {
        return Ok(GitStashResult {
            status: GitStashStatus::Gone,
            ..Default::default()
        });
    };
    let output = crate::git_cli::run(&workdir, &["stash", "drop", &spec])
        .await
        .map_err(|e| RpcError::internal(format!("running git stash drop: {e}")))?;
    // The picker that fired this is still open on a list that now has one fewer entry — refresh it
    // before answering, or the next keystroke acts on a row that no longer exists.
    let pushes = {
        let mut s = state.lock().await;
        refresh_git_ref_pickers(&mut s, PickerKind::GitStash)
    };
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    if !output.success() {
        return Ok(GitStashResult {
            status: GitStashStatus::Refused,
            message: output.stderr.trim_end().to_string(),
            ..Default::default()
        });
    }
    Ok(GitStashResult {
        status: GitStashStatus::Dropped,
        message: output.stdout.trim_end().to_string(),
        ..Default::default()
    })
}

/// The `stash@{n}` the CLI needs, resolved from the stable hash the client sent. `None` when the
/// entry is gone — dropped or popped elsewhere while the picker was up. Resolving here rather than
/// trusting the listed position is what stops a shifted index acting on the *wrong* stash.
async fn stash_spec(workdir: &std::path::Path, oid: &str) -> Option<String> {
    let (workdir, oid) = (workdir.to_path_buf(), oid.to_string());
    tokio::task::spawn_blocking(move || crate::git::stash_index_of(&workdir, &oid))
        .await
        .ok()
        .flatten()
        .map(|index| format!("stash@{{{index}}}"))
}

/// Delete a local branch. No tree move, so none of checkout's machinery applies.
pub async fn git_delete_branch(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: GitDeleteBranchParams,
) -> Result<GitDeleteBranchResult, RpcError> {
    let workdir = {
        let s = state.lock().await;
        let repo =
            resolve_writable_repo(&s, ctx.client_id, params.repo_id.as_ref(), params.buffer_id)?;
        std::path::PathBuf::from(&repo.repo_id)
    };

    // Classify the two refusals worth naming ourselves, from libgit2. Both are things git would
    // also refuse, but the client acts differently on each — `NotMerged` escalates to a force
    // confirm — and deriving that from git's wording is what decision 1 rules out.
    let branch = params.branch.clone();
    let classified = tokio::task::spawn_blocking({
        let workdir = workdir.clone();
        let branch = branch.clone();
        let force = params.force;
        move || {
            let head_is_branch = matches!(
                crate::git::discover_repo(&workdir).map(|i| i.head),
                Some(GitHead::Branch { ref name, .. }) if *name == branch
            );
            if head_is_branch {
                return Some(GitDeleteBranchStatus::IsCurrentBranch);
            }
            // Force is the user having already seen and accepted the loss, so skip the check.
            if !force && !crate::git::branch_is_merged(&workdir, &branch) {
                return Some(GitDeleteBranchStatus::NotMerged);
            }
            None
        }
    })
    .await
    .unwrap_or(None);
    if let Some(status) = classified {
        return Ok(GitDeleteBranchResult {
            status,
            message: String::new(),
        });
    }

    let flag = if params.force { "-D" } else { "-d" };
    let outcome = crate::git_cli::run(&workdir, &["branch", flag, &params.branch]).await;
    let output = match outcome {
        Ok(o) => o,
        Err(e) => return Err(RpcError::internal(format!("running git branch: {e}"))),
    };
    if !output.success() {
        let message = if output.stderr.trim().is_empty() {
            output.stdout
        } else {
            output.stderr
        };
        return Ok(GitDeleteBranchResult {
            status: GitDeleteBranchStatus::Refused,
            message: message.trim_end().to_string(),
        });
    }
    // The branch picker is still open on a list that now has one fewer row — same reason the stash
    // drop refreshes. (Checkout doesn't need this: it closes its picker.)
    let pushes = {
        let mut s = state.lock().await;
        refresh_git_ref_pickers(&mut s, PickerKind::GitBranches)
    };
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    Ok(GitDeleteBranchResult {
        status: GitDeleteBranchStatus::Deleted,
        message: String::new(),
    })
}

/// The diff baseline in force for `workdir`, or `None` for the default (the index).
///
/// A one-line lock read, given a name because it is asked from the paths that generate a
/// working-changes view — all of which then do their work *off* the lock, and none of which should
/// be reaching into `git_baseline_choices` by hand at the point of use.
pub async fn baseline_choice(
    state: &SharedState,
    workdir: &std::path::Path,
) -> Option<GitBaselineSource> {
    state
        .lock()
        .await
        .git_baseline_choices
        .get(workdir)
        .cloned()
}

/// The companion to [`reconcile_repo`], run once its lock is released.
///
/// That pass brings the repo's *file* buffers back in line with the tree it just moved; this does
/// the same for the buffer whose entire content is a picture of that tree. Separate rather than
/// folded in because the rebuild needs a `git diff`, which has no business under the state lock.
async fn reconcile_working_changes(state: &SharedState, workdir: &std::path::Path) {
    refresh_working_changes_views(state, &std::iter::once(workdir.to_path_buf()).collect()).await;
}

/// The reconciliation pass itself, separated from the RPC so an editor-driven operation that has
/// just rewritten the tree (commit, and later checkout) runs exactly the same one rather than a
/// near-copy that drifts. Returns the summary plus the pushes to send once the lock is released.
fn reconcile_repo(
    s: &mut ServerState,
    workdir: &std::path::Path,
) -> (GitRefreshResult, PendingPushes) {
    let mut affected: Vec<BufferId> = s
        .git_baseline
        .iter()
        .filter(|(_, b)| b.repo.as_ref().is_some_and(|r| r.workdir == workdir))
        .map(|(id, _)| *id)
        .collect();
    // `git_baseline` is a `HashMap`; sort so the reported lists are stable.
    affected.sort_unstable();

    let mut result = GitRefreshResult::default();
    let mut pushes: PendingPushes = Vec::new();

    for id in affected {
        let Some((path, dirty, recorded_mtime, was_deleted)) = s.try_doc_of(id).map(|d| {
            (
                d.canonical_path.clone(),
                d.dirty,
                d.last_modified_unix_ms,
                d.externally_deleted,
            )
        }) else {
            continue;
        };
        let Some(path) = path else {
            continue; // scratch: no file to reconcile against
        };

        let disk_mtime = std::fs::metadata(&path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64);

        if disk_mtime.is_none() {
            // Gone from the working tree — checked out a ref without this file. The buffer stays
            // open with its content intact; closing it would destroy work the user can still save.
            if !was_deleted {
                if let Some(doc) = s.try_doc_of_mut(id) {
                    doc.externally_deleted = true;
                }
                pushes.extend(collect_buffer_state_pushes(s, id));
            }
            result.missing.push(id);
        } else if disk_mtime == recorded_mtime && !was_deleted {
            // Byte-identical to what we already hold (this file wasn't part of the move, or the
            // move restored it). Nothing to reload — but the baseline below still refreshes.
        } else if dirty {
            // Unsaved edits *and* the file moved underneath: the one case with no safe automatic
            // answer, so it's surfaced rather than resolved. Same flag `buffer/save` and
            // `buffer/reload` already understand.
            if let Some(doc) = s.try_doc_of_mut(id) {
                doc.externally_modified = true;
                doc.externally_deleted = false;
            }
            pushes.extend(collect_buffer_state_pushes(s, id));
            result.diverged.push(id);
        } else {
            match reload_buffer_locked(s, id) {
                Ok((_, reload_pushes)) => {
                    pushes.extend(reload_pushes);
                    result.reloaded.push(id);
                }
                // A file that exists but can't be read (permissions, a directory in its place)
                // shouldn't abort the whole repo's reconciliation.
                Err(e) => tracing::warn!(?id, error = ?e, "reload during git/refresh failed"),
            }
        }

        // Unconditional, including for buffers just reloaded above: a tree move takes HEAD with
        // it, so the cached blobs need re-reading, and the reload path only re-diffs against the
        // blobs it already had.
        pushes.extend(refresh_git_for_buffer(s, id));
    }

    // A tree move changes explorer entry colours and buffer-picker status dots without
    // necessarily touching any *open* buffer, so refresh those too — the same follow-up the
    // watcher does for an externally-driven Git change.
    let workdirs: std::collections::HashSet<std::path::PathBuf> =
        [workdir.to_path_buf()].into_iter().collect();
    let dirs: std::collections::HashSet<std::path::PathBuf> =
        explorer_dirs_in_workdirs(s, &workdirs)
            .into_iter()
            .collect();
    pushes.extend(refresh_explorers_for_dirs(s, &dirs));
    pushes.extend(refresh_view_pickers(s));

    (result, pushes)
}

/// A long-running git operation, announced to every client while it runs.
///
/// Holds the cancel handle in [`ServerState::git_operations`], streams git's progress lines out as
/// `git/operation_changed` pushes, and clears the indicator on drop-in-name-only (`finish`, which
/// has to be awaited, so it's an explicit call rather than a `Drop` impl).
struct OperationReporter {
    workdir: std::path::PathBuf,
    /// The most recent progress line, overwritten as fast as git produces them and drained by the
    /// ticker. Coalescing here rather than pushing per line is the point: `git push` emits a
    /// counter update per percent, and a notification per percent to every client is a lot of
    /// traffic to render the same word.
    latest: Arc<std::sync::Mutex<Option<String>>>,
    ticker: tokio::task::JoinHandle<()>,
}

/// How often the in-flight indicator is refreshed. Fast enough to read as live, slow enough that a
/// busy transfer doesn't turn into a push storm.
const OPERATION_TICK: std::time::Duration = std::time::Duration::from_millis(200);

/// Announce `kind` starting in `workdir`, and hand back the token its runner should watch.
async fn begin_operation(
    state: &SharedState,
    workdir: &Path,
    kind: GitOperationKind,
) -> (crate::git_cli::CancelToken, OperationReporter) {
    let (handle, token) = crate::git_cli::cancel_channel();
    {
        let mut s = state.lock().await;
        s.git_operations.insert(workdir.to_path_buf(), handle);
    }
    push_operation(
        state,
        workdir,
        Some(GitOperation {
            kind,
            detail: String::new(),
        }),
    )
    .await;

    let latest = Arc::new(std::sync::Mutex::new(None::<String>));
    let ticker = tokio::spawn({
        let state = state.clone();
        let workdir = workdir.to_path_buf();
        let latest = latest.clone();
        async move {
            loop {
                tokio::time::sleep(OPERATION_TICK).await;
                let detail = latest.lock().ok().and_then(|mut l| l.take());
                let Some(detail) = detail else {
                    continue; // nothing new since the last tick
                };
                push_operation(&state, &workdir, Some(GitOperation { kind, detail })).await;
            }
        }
    });
    (
        token,
        OperationReporter {
            workdir: workdir.to_path_buf(),
            latest,
            ticker,
        },
    )
}

impl OperationReporter {
    /// The progress sink for [`crate::git_cli::run_streaming`] — synchronous by necessity (it runs
    /// inside the read loop), so it only ever parks the line for the ticker to send.
    fn sink(&self) -> impl FnMut(String) + Send {
        let latest = self.latest.clone();
        move |line| {
            if let Ok(mut slot) = latest.lock() {
                *slot = Some(line);
            }
        }
    }

    /// Stop reporting and clear every client's indicator.
    async fn finish(self, state: &SharedState) {
        self.ticker.abort();
        {
            let mut s = state.lock().await;
            s.git_operations.remove(&self.workdir);
        }
        push_operation(state, &self.workdir, None).await;
    }
}

/// Push one `git/operation_changed` to every connected client. Repo-scoped rather than
/// client-scoped: any client with this repo open wants the indicator, and the operation isn't
/// owned by whoever happened to start it.
async fn push_operation(state: &SharedState, workdir: &Path, operation: Option<GitOperation>) {
    let params = GitOperationChangedParams {
        repo_id: path_string(workdir),
        operation,
    };
    let value = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
    let pushes: PendingPushes = {
        let s = state.lock().await;
        s.clients
            .values()
            .map(|sess| {
                (
                    sess.outbound.clone(),
                    Notification {
                        jsonrpc: JsonRpc,
                        method: GitOperationChanged::NAME.into(),
                        params: value.clone(),
                    },
                )
            })
            .collect()
    };
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
}

/// Run one network git command, optionally announcing it as a cancellable operation.
///
/// The single place fetch and push agree on how a network invocation behaves, so "announced and
/// cancellable" versus "silent" is one argument rather than two divergent code paths. Returns
/// git's output and whether the user cancelled — read from the token rather than inferred from the
/// exit status, since a killed git looks identical to a crashed one from the outside.
async fn run_network_git(
    state: &SharedState,
    workdir: &Path,
    args: &[&str],
    announce: Option<GitOperationKind>,
) -> std::io::Result<(crate::git_cli::GitOutput, bool)> {
    let Some(kind) = announce else {
        return Ok((crate::git_cli::run(workdir, args).await?, false));
    };
    let (token, reporter) = begin_operation(state, workdir, kind).await;
    let output = crate::git_cli::run_streaming(workdir, args, token.clone(), reporter.sink()).await;
    reporter.finish(state).await;
    let cancelled = *token.borrow();
    Ok((output?, cancelled))
}

/// Stop the long-running git operation in a repo — see [`GitCancel`].
pub async fn git_cancel(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: GitCancelParams,
) -> Result<GitCancelResult, RpcError> {
    let s = state.lock().await;
    // Resolved (not just trusted) so a client can't kill an operation in a repo it can't see.
    let repo = resolve_repo(&s, ctx.client_id, &params.repo_id)?;
    let cancelled = match s.git_operations.get(Path::new(&repo.repo_id)) {
        // A send failure means the runner has already gone, which is the same race as no entry.
        Some(handle) => handle.send(true).is_ok(),
        None => false,
    };
    Ok(GitCancelResult { cancelled })
}

/// The repos the periodic fetcher may fetch: everything reachable from a *connected* client's
/// active workspace, one entry per repository.
///
/// Three filters, each load-bearing:
///
/// - **Connected clients only.** No client means no one to show a count to, and the daemon is
///   auto-started and idle-reaped — it has no business waking the network on its own behalf.
/// - **`roots` non-empty**, the same reachability guard the write operations use. A repo reached
///   only through an open buffer is a dependency checkout a goto-definition landed in; fetching it
///   would be contacting a remote the user has never expressed any interest in.
/// - **One per `common_dir`.** Worktrees of one repository share their object store and remote
///   refs, so fetching each in turn is the same fetch two or three times over.
pub(crate) fn auto_fetch_targets(s: &ServerState) -> Vec<GitRepoInfo> {
    let mut out: Vec<GitRepoInfo> = Vec::new();
    let mut client_ids: Vec<ClientId> = s.clients.keys().copied().collect();
    client_ids.sort_unstable(); // `clients` is a HashMap; keep the order deterministic
    for client_id in client_ids {
        // A client that hasn't activated a workspace yet (still on the boot chooser) contributes
        // nothing rather than failing the sweep.
        let Ok(repos) = reachable_repos(s, client_id) else {
            continue;
        };
        for repo in repos {
            if repo.roots.is_empty() {
                continue;
            }
            if out.iter().any(|r| r.common_dir == repo.common_dir) {
                continue;
            }
            out.push(repo);
        }
    }
    out.sort_by(|a, b| a.repo_id.cmp(&b.repo_id));
    out
}

/// Fetch from the remote, refreshing the divergence counts every open buffer in the repo carries.
///
/// The repo is already resolved: [`git_fetch`] resolves it from a client's request, the periodic
/// fetcher ([`crate::server::git_fetch_loop`]) from the workspaces it can reach. Everything after
/// that point — the no-remote gate, the spawn, the refresh — has to be identical for both, so it
/// lives here rather than in either caller.
pub(crate) async fn fetch_repo(
    state: &SharedState,
    workdir: std::path::PathBuf,
    announce: bool,
) -> Result<GitFetchResult, RpcError> {
    // Ask before spawning. A repo with no remote is the one failure that retrying can never fix,
    // and the periodic fetcher needs to tell it apart from a network blip that will.
    let has_remote = {
        let workdir = workdir.clone();
        tokio::task::spawn_blocking(move || crate::git::has_remote(&workdir))
            .await
            .unwrap_or(false)
    };
    if !has_remote {
        return Ok(GitFetchResult {
            status: GitFetchStatus::NoRemote,
            ..Default::default()
        });
    }

    // **No watcher suppression here**, unlike every other git mutation — deliberately, and it
    // would be a bug. Suppression is keyed by workdir *containment*, so holding it across a fetch
    // (which can take many seconds on a slow link) would swallow the user's own edits to their own
    // files for the duration. Nothing needs it: a fetch writes `refs/remotes/**` and objects,
    // never a file in the working tree, so there is no storm to absorb and no reconciliation pass
    // whose report a racing watcher could steal. The ref writes the watcher *does* see are exactly
    // the signal that keeps a terminal `git fetch` reflected here too.
    //
    // `announce` is what separates the two callers: `Space g f` puts an indicator up and can be
    // cancelled, while the periodic fetcher runs silent and uninterruptible. A background operation
    // that raised a spinner every quarter of an hour would be exactly the interruption it exists to
    // avoid, and there is nobody waiting on it to offer a cancel to.
    let (output, cancelled) = match run_network_git(
        state,
        &workdir,
        &["fetch"],
        announce.then_some(GitOperationKind::Fetch),
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return Err(RpcError::internal(format!("running git fetch: {e}"))),
    };
    if cancelled {
        return Ok(GitFetchResult {
            status: GitFetchStatus::Cancelled,
            ..Default::default()
        });
    }
    if !output.success() {
        let message = if output.stderr.trim().is_empty() {
            output.stdout
        } else {
            output.stderr
        };
        return Ok(GitFetchResult {
            status: GitFetchStatus::Refused,
            message: message.trim_end().to_string(),
            upstream: None,
        });
    }

    let pushes = {
        let mut s = state.lock().await;
        refresh_repo_baselines(&mut s, &workdir)
    };
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    // Read back outside the lock: this is a revwalk, and it's the answer the caller reports.
    let upstream = tokio::task::spawn_blocking(move || crate::git::repo_upstream(&workdir))
        .await
        .ok()
        .flatten();
    Ok(GitFetchResult {
        status: GitFetchStatus::Fetched,
        message: String::new(),
        upstream,
    })
}

/// Fetch from the remote — see [`GitFetch`]. Reachability-gated: this reaches the network on the
/// user's behalf, so a repo they never opened (reached only through a buffer) is refused, the same
/// guard the write operations use.
pub async fn git_fetch(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: GitFetchParams,
) -> Result<GitFetchResult, RpcError> {
    let workdir = {
        let s = state.lock().await;
        let repo =
            resolve_writable_repo(&s, ctx.client_id, params.repo_id.as_ref(), params.buffer_id)?;
        std::path::PathBuf::from(&repo.repo_id)
    };
    fetch_repo(state, workdir, true).await
}

/// What a network operation needs to know about a repo before it can decide anything, read in one
/// pass off the async thread: where HEAD points, how far it has diverged, and what remotes exist.
///
/// Shared by push and pull because they ask the same three questions and answer them differently —
/// a branch with no upstream is push's cue to set one and pull's reason to refuse.
struct RemotePreflight {
    head: Option<GitHead>,
    upstream: Option<GitUpstreamStatus>,
    remotes: Vec<String>,
}

fn remote_preflight(workdir: &Path) -> RemotePreflight {
    RemotePreflight {
        head: crate::git::discover_repo(workdir).map(|i| i.head),
        upstream: crate::git::repo_upstream(workdir),
        remotes: crate::git::remote_names(workdir),
    }
}

/// Publish the current branch — see [`GitPush`]. Reachability-gated like the other network
/// operation.
pub async fn git_push(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: GitPushParams,
) -> Result<GitPushResult, RpcError> {
    let workdir = {
        let s = state.lock().await;
        let repo =
            resolve_writable_repo(&s, ctx.client_id, params.repo_id.as_ref(), params.buffer_id)?;
        std::path::PathBuf::from(&repo.repo_id)
    };

    // Everything decidable without the network is decided here. Three of the four refusals below
    // will never succeed on retry, and the fourth ("nothing to push") would spend a round trip to
    // be told what we already know.
    let pre = {
        let workdir = workdir.clone();
        tokio::task::spawn_blocking(move || remote_preflight(&workdir))
            .await
            .map_err(|e| RpcError::internal(format!("reading repo state: {e}")))?
    };
    let refuse = |status| {
        Ok(GitPushResult {
            status,
            ..Default::default()
        })
    };
    if pre.remotes.is_empty() {
        return refuse(GitPushStatus::NoRemote);
    }
    let branch = match &pre.head {
        Some(GitHead::Branch { name, .. }) => name.clone(),
        // No commits yet, so there is nothing to publish — answered here rather than spending a
        // round trip to have git say "src refspec does not match any", which reads like a fault.
        Some(GitHead::Unborn { .. }) => return refuse(GitPushStatus::NothingToPush),
        Some(GitHead::Detached { .. }) => return refuse(GitPushStatus::DetachedHead),
        None => return refuse(GitPushStatus::Refused),
    };

    // The argv is the whole of the branch/upstream decision, so it's made in one place. `-u` on a
    // branch that has never been pushed is what gives the divergence counts something to compare
    // against from then on; with several remotes and no upstream, which one to publish to is the
    // user's call rather than ours.
    let set_upstream = pre.upstream.is_none();
    let args: Vec<String> = if set_upstream {
        let [remote] = &pre.remotes[..] else {
            return refuse(GitPushStatus::AmbiguousRemote);
        };
        vec![
            "push".into(),
            "--set-upstream".into(),
            remote.clone(),
            branch,
        ]
    } else {
        if pre.upstream.as_ref().is_some_and(|u| u.ahead == 0) {
            return Ok(GitPushResult {
                status: GitPushStatus::NothingToPush,
                upstream: pre.upstream,
                ..Default::default()
            });
        }
        vec!["push".into()]
    };

    // No watcher suppression and no dirty-buffer pre-flight, for the same reason as `fetch_repo`:
    // a push writes remote refs, never the working tree.
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let (output, cancelled) =
        match run_network_git(state, &workdir, &argv, Some(GitOperationKind::Push)).await {
            Ok(v) => v,
            Err(e) => return Err(RpcError::internal(format!("running git push: {e}"))),
        };
    if cancelled {
        return Ok(GitPushResult {
            status: GitPushStatus::Cancelled,
            ..Default::default()
        });
    }

    // Re-read either way: a successful push moved the remote-tracking ref, and a failed one is
    // about to be classified by how far behind we are.
    let after = {
        let workdir = workdir.clone();
        tokio::task::spawn_blocking(move || crate::git::repo_upstream(&workdir))
            .await
            .ok()
            .flatten()
    };

    if !output.success() {
        let message = if output.stderr.trim().is_empty() {
            output.stdout
        } else {
            output.stderr
        };
        // The fast-forward rule, named from *our* read of the graph rather than from git's wording
        // — see [`GitPushStatus::Behind`] for why this is classified after the attempt instead of
        // refused before it. A rejection with nothing behind us is something else entirely
        // (credentials, a protected branch, a pre-receive hook) and keeps git's own text.
        let status = match after.as_ref() {
            Some(u) if u.behind > 0 => GitPushStatus::Behind,
            _ => GitPushStatus::Refused,
        };
        return Ok(GitPushResult {
            status,
            message: message.trim_end().to_string(),
            upstream: after,
            set_upstream: false,
        });
    }

    let pushes = {
        let mut s = state.lock().await;
        refresh_repo_baselines(&mut s, &workdir)
    };
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    Ok(GitPushResult {
        status: GitPushStatus::Pushed,
        message: String::new(),
        upstream: after,
        set_upstream,
    })
}

/// Bring the current branch up to date — see [`GitPull`]. The one operation that is both a network
/// call and a tree move, so it carries both sets of machinery.
pub async fn git_pull(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: GitPullParams,
) -> Result<GitPullResult, RpcError> {
    let (workdir, git_dir) = {
        let s = state.lock().await;
        let repo =
            resolve_writable_repo(&s, ctx.client_id, params.repo_id.as_ref(), params.buffer_id)?;
        (
            std::path::PathBuf::from(&repo.repo_id),
            std::path::PathBuf::from(&repo.git_dir),
        )
    };

    let (pre, operation, conflicts) = {
        let workdir = workdir.clone();
        tokio::task::spawn_blocking(move || {
            (
                remote_preflight(&workdir),
                crate::git::repo_operation(&workdir),
                crate::git::conflicted_paths(&workdir),
            )
        })
        .await
        .map_err(|e| RpcError::internal(format!("reading repo state: {e}")))?
    };
    let refuse = |status| {
        Ok(GitPullResult {
            status,
            ..Default::default()
        })
    };
    // The "pulling is impossible here" refusals come before the "pulling isn't safe right now" one:
    // telling someone to go and save three buffers for an operation that could never have run is a
    // wasted instruction. All of these are knowable without the network.
    if pre.remotes.is_empty() {
        return refuse(GitPullStatus::NoRemote);
    }
    // **Before the detached-HEAD check, deliberately.** A stopped rebase detaches HEAD, so testing
    // that first would answer "not on a branch" — the symptom — to a user who is mid-rebase from a
    // pull that conflicted a minute ago, and say nothing about the conflict they still have to
    // resolve. Reported with the conflicting paths, which is the same answer the conflicted pull
    // gave and the one still worth acting on.
    if let Some(operation) = operation {
        return Ok(GitPullResult {
            status: GitPullStatus::OperationInProgress,
            operation: Some(operation),
            conflicts,
            ..Default::default()
        });
    }
    match &pre.head {
        Some(GitHead::Branch { .. }) => {}
        Some(GitHead::Detached { .. }) => return refuse(GitPullStatus::DetachedHead),
        // No commits, so no tracking information either — the same answer, and one the user fixes
        // the same way.
        Some(GitHead::Unborn { .. }) => return refuse(GitPullStatus::NoUpstream),
        None => return refuse(GitPullStatus::Refused),
    }
    if pre.upstream.is_none() {
        return refuse(GitPullStatus::NoUpstream);
    }

    // Git cannot see an unsaved buffer: it guards files on disk, so it would merge underneath one
    // and leave the user's edits sitting on a base that no longer exists. Refused rather than saved
    // on their behalf, exactly as checkout does.
    let blocked = {
        let s = state.lock().await;
        dirty_buffers_in_repo(&s, &workdir)
    };
    if !blocked.is_empty() {
        return Ok(GitPullResult {
            status: GitPullStatus::BlockedByDirtyBuffers,
            blocked,
            ..Default::default()
        });
    }

    // Where HEAD was, so the outcome can be read from the graph afterwards rather than from git's
    // summary line. Taken before the run and compared after: that difference is the whole of the
    // fast-forward / merge / rebase classification.
    let before = {
        let workdir = workdir.clone();
        tokio::task::spawn_blocking(move || crate::git::head_oid(&workdir))
            .await
            .ok()
            .flatten()
    };

    // Plain `git pull`. No `--ff-only`, no `--rebase`: the user's own config decides, which is
    // decision 1 applied to strategy rather than to hooks. An editor that forced one would refuse
    // where the user's terminal would have succeeded.
    let run = run_tree_git(
        state,
        &workdir,
        &["pull"],
        Some(GitOperationKind::Pull),
        None,
    )
    .await?;

    // One blocking pass for everything the outcome is read from: the divergence as it now stands,
    // whatever the index is left conflicted on, how HEAD moved, and — only for the refusal
    // classification below — whether the working tree is clean.
    let (upstream, conflicts, moved, clean, stopped_in) = {
        let workdir = workdir.clone();
        let before = before.clone();
        tokio::task::spawn_blocking(move || {
            (
                crate::git::repo_upstream(&workdir),
                crate::git::conflicted_paths(&workdir),
                crate::git::head_move(&workdir, before.as_deref()),
                crate::git::changed_files_in_repo(&workdir).is_empty(),
                crate::git::repo_operation(&workdir),
            )
        })
        .await
        .map_err(|e| RpcError::internal(format!("reading repo state: {e}")))?
    };

    // Conflicts first, ahead of both cancellation and the generic refusal. Git exits non-zero when
    // a merge stops on one, but the *state the repo is in* is the fact the user has to act on, and
    // "cancelled" or a wall of stderr would bury it. This also catches a merge the user began in a
    // terminal and never finished: the pull refused because of it, and naming the conflicted files
    // is a better account of why than git's "you have not concluded your merge".
    if !conflicts.is_empty() {
        return Ok(GitPullResult {
            status: GitPullStatus::Conflicted,
            message: git_failure_message(&run.output),
            upstream,
            refreshed: run.refreshed,
            conflicts,
            // Which of the two stopped is the user's next question — `git merge --abort` and
            // `git rebase --abort` are different commands — and it comes from the repo rather
            // than from guessing at their `pull.rebase`.
            operation: stopped_in,
            ..Default::default()
        });
    }
    if run.cancelled {
        return Ok(GitPullResult {
            status: GitPullStatus::Cancelled,
            upstream,
            refreshed: run.refreshed,
            operation: stopped_in,
            // Cancelling SIGKILLs git, and the merge half writes the index — so unlike a cancelled
            // fetch or push this one can leave a lock that makes every later git operation fail.
            // Checked only here: the lock is normal and transient while git is *running*, so
            // looking for it on a completed run would report a race as a fault.
            index_locked: git_dir.join("index.lock").exists(),
            ..Default::default()
        });
    }

    if !run.output.success() {
        // Both branches moved and git declined to guess how to reconcile them — classified from our
        // own read of the graph, the same trick [`GitPushStatus::Behind`] uses and for the same
        // reason: whether git refuses depends on the user's `pull.rebase`/`pull.ff` config, so
        // predicting it would mean re-implementing git's config precedence.
        //
        // **Gated on a clean tree**, which is what keeps it honest. A diverged branch *and*
        // uncommitted changes the merge would overwrite is a different refusal with the same
        // ahead/behind signature, and its stderr names the files — telling that user to configure a
        // pull strategy would send them after the wrong problem.
        let diverged = clean
            && upstream
                .as_ref()
                .is_some_and(|u| u.ahead > 0 && u.behind > 0);
        return Ok(GitPullResult {
            status: if diverged {
                GitPullStatus::Diverged
            } else {
                GitPullStatus::Refused
            },
            message: git_failure_message(&run.output),
            upstream,
            refreshed: run.refreshed,
            ..Default::default()
        });
    }

    Ok(GitPullResult {
        status: match moved {
            crate::git::HeadMove::Unchanged => GitPullStatus::UpToDate,
            crate::git::HeadMove::FastForward => GitPullStatus::FastForwarded,
            crate::git::HeadMove::Merge => GitPullStatus::Merged,
            crate::git::HeadMove::Rewritten => GitPullStatus::Rebased,
        },
        upstream,
        refreshed: run.refreshed,
        ..Default::default()
    })
}

/// Re-read the Git baseline of every open buffer in `workdir`, collecting the resulting pushes.
///
/// Deliberately *not* [`reconcile_repo`]. That exists for operations that rewrite the working tree
/// and pays accordingly — an mtime stat and a possible reload per buffer, plus explorer and
/// buffer-picker refreshes. A fetch moves no file and changes no file's status against HEAD or the
/// index, so all of that would be work with no observable effect, on a path the periodic fetcher
/// runs unattended for as long as the editor is open. What a fetch *does* change is
/// `refs/remotes/**`, and the baseline's cached upstream divergence is read from exactly there.
fn refresh_repo_baselines(s: &mut ServerState, workdir: &Path) -> PendingPushes {
    let mut affected: Vec<BufferId> = s
        .git_baseline
        .iter()
        .filter(|(_, b)| b.repo.as_ref().is_some_and(|r| r.workdir == workdir))
        .map(|(id, _)| *id)
        .collect();
    affected.sort_unstable(); // `git_baseline` is a HashMap; keep push order stable
    let mut pushes: PendingPushes = Vec::new();
    for id in affected {
        pushes.extend(refresh_git_for_buffer(s, id));
    }
    pushes
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn repo_info(identity: &crate::git::RepoIdentity) -> GitRepoInfo {
    GitRepoInfo {
        repo_id: path_string(&identity.workdir),
        git_dir: path_string(&identity.git_dir),
        common_dir: path_string(&identity.common_dir),
        head: identity.head.clone(),
        roots: Vec::new(),
    }
}

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
        .try_doc_of(params.buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
    // A file at a revision blames too, from the `(repo, rev, path)` its key names. Only a buffer
    // that is neither — a scratch, or a commit's whole patch — has nothing to attribute.
    let revision_file = revision_file_of(buf);
    if buf.canonical_path.is_none() && revision_file.is_none() {
        return Ok(GitBlameLineResult {
            blame: None,
            commit_info: None,
        });
    }
    let blame = cursor_line_blame(&mut s, params.buffer_id, params.line);
    // Composite post-step: resolve the commit's details in the same round-trip. Best-effort — an
    // unresolvable hash just yields `None`.
    let commit_info = match &blame {
        Some(b) if params.include_commit_info && !b.is_uncommitted => match &revision_file {
            // No cached baseline to borrow the repo from, so name it from the key.
            Some((workdir, rel_path, _)) => crate::git::commit_info(
                &crate::git::GitRepo {
                    workdir: workdir.clone(),
                    rel_path: rel_path.clone(),
                },
                &b.commit,
            ),
            None => s
                .git_baseline
                .get(&params.buffer_id)
                .and_then(|base| base.repo.as_ref())
                .and_then(|repo| crate::git::commit_info(repo, &b.commit)),
        },
        _ => None,
    };
    Ok(GitBlameLineResult { blame, commit_info })
}

/// Resolve one line's blame from the per-revision whole-file cache, (re)computing the cache when
/// stale. Shared by the `git/blame_line` request (the commit-details popover) and the
/// blame-follow refresher (`spawn_blame_refresh`). `None` for a missing buffer, no repo, an
/// untracked file, or a line past end-of-file.
/// `(workdir, repo-relative path, rev)` for a **file at a revision** — the buffers
/// `git/show <rev>:<path>` materialises.
///
/// `None` for everything else, including a *commit's* patch: that names many files and is not one
/// of them, so nothing here can be asked of it.
fn revision_file_of(doc: &Document) -> Option<(std::path::PathBuf, std::path::PathBuf, String)> {
    let target = &doc.virtual_source.as_ref()?.target;
    Some((
        std::path::PathBuf::from(&target.repo_id),
        std::path::PathBuf::from(target.path()?),
        target.rev()?.to_string(),
    ))
}

fn cursor_line_blame(
    s: &mut ServerState,
    buffer_id: BufferId,
    line: u32,
) -> Option<aether_protocol::git::BlameInfo> {
    let buf = s.try_doc_of(buffer_id)?;
    let revision = buf.revision;
    let stale = s
        .git_blame
        .get(&buffer_id)
        .is_none_or(|c| c.revision != revision);
    if stale {
        // Blame via the cached repo (no rediscovery). `None` repo (untracked / no repo) → empty.
        // The `buf`/`git_baseline` borrows end at the `compute_blame` call; `lines` is owned, so
        // the `git_blame` mutation below is free of them.
        let lines = match revision_file_of(buf) {
            // A file at a revision has no baseline (nothing registers one) and no working-tree
            // content to reconcile — it blames at the revision its key names.
            Some((workdir, rel_path, rev)) => {
                crate::git::compute_blame_at(&workdir, &rel_path, &rev, &buf.text)
                    .unwrap_or_default()
            }
            None => match s.git_baseline.get(&buffer_id).and_then(|b| b.repo.as_ref()) {
                Some(repo) => crate::git::compute_blame(repo, &buf.text).unwrap_or_default(),
                None => Vec::new(),
            },
        };
        s.git_blame
            .insert(buffer_id, BlameCache { revision, lines });
    }
    s.git_blame
        .get(&buffer_id)
        .and_then(|c| c.lines.get(line as usize).cloned().flatten())
}

/// Settle window for blame-follow pushes. Matches [`SYMBOL_HIGHLIGHT_DEBOUNCE`] so both
/// cursor-following decorations land together once the cursor rests, and a held `j` produces no
/// blame traffic at all.
const BLAME_FOLLOW_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(120);

/// `git/set_blame_follow` — toggle server-driven cursor-line blame for `(client, buffer)`.
/// Enabling arms an immediate (still debounced) refresh so the label appears without waiting for
/// a cursor move; disabling drops all follow state — the client clears its own label locally.
pub async fn git_set_blame_follow(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: GitSetBlameFollowParams,
) -> Result<(), RpcError> {
    let client_id = ctx.client_id;
    let key = (client_id, params.buffer_id);
    let armed = {
        let mut s = state.lock().await;
        if params.enabled {
            s.blame_follow.insert(key);
            // Forget the last push so the first settle always delivers a label — re-enabling
            // after an Insert round-trip must refresh even at an unchanged position.
            s.blame_last_pushed.remove(&key);
            let gen = s.blame_follow_gen.entry(key).or_insert(0);
            *gen += 1;
            Some(*gen)
        } else {
            s.blame_follow.remove(&key);
            s.blame_follow_gen.remove(&key);
            s.blame_last_pushed.remove(&key);
            None
        }
    };
    if let Some(epoch) = armed {
        let token = state.lock().await.deferred.start();
        spawn_blame_refresh(state.clone(), client_id, params.buffer_id, epoch, token);
    }
    Ok(())
}

/// The debounced body of blame-follow: wait out the settle window, then — if this is still the
/// latest arming — resolve the followed cursor's line, look up its blame (per-revision cache),
/// and push `git/blame_changed` unless the settled `(line, revision)` matches the last push.
/// Mirrors [`spawn_symbol_highlight_refresh`].
pub fn spawn_blame_refresh(
    state: SharedState,
    client_id: ClientId,
    buffer_id: BufferId,
    epoch: u64,
    token: DeferredToken,
) {
    tokio::spawn(async move {
        let _token = token;
        tokio::time::sleep(BLAME_FOLLOW_DEBOUNCE).await;
        let key = (client_id, buffer_id);
        let (sender, notif) = {
            let mut s = state.lock().await;
            if s.blame_follow_gen.get(&key) != Some(&epoch) || !s.blame_follow.contains(&key) {
                return; // superseded by a newer move, or unfollowed while we slept
            }
            // No cursor entry yet (followed straight after open, before any cursor RPC) reads
            // as the origin — matching every other handler's missing-cursor default.
            let line = s.cursors.get(&key).map(|c| c.position.line).unwrap_or(0);
            let Some(buf) = s.try_doc_of(buffer_id) else {
                return;
            };
            // A file at a revision blames from its key, like `git/blame_line`. Only a buffer that
            // is neither a file nor one of those — a scratch, or a commit's whole patch — has
            // nothing to attribute.
            if buf.canonical_path.is_none() && revision_file_of(buf).is_none() {
                return;
            }
            let revision = buf.revision;
            if s.blame_last_pushed.get(&key) == Some(&(line, revision)) {
                return; // blame is deterministic per (line, revision): nothing new to say
            }
            let blame = cursor_line_blame(&mut s, buffer_id, line);
            s.blame_last_pushed.insert(key, (line, revision));
            let Some(sender) = s.clients.get(&client_id).map(|c| c.outbound.clone()) else {
                return;
            };
            let params = GitBlameChangedParams {
                buffer_id,
                line,
                blame,
            };
            (
                sender,
                Notification {
                    jsonrpc: JsonRpc,
                    method: GitBlameChanged::NAME.into(),
                    params: serde_json::to_value(params).unwrap_or(serde_json::Value::Null),
                },
            )
        };
        let _ = sender.send(notif).await;
    });
}

/// Drain cursor-change events from [`set_cursor`] and re-arm the debounced refresh of each
/// cursor-following decoration (blame label, symbol highlights). A dedicated task — rather than
/// arming at the `set_cursor` call sites — because arming spawns with a `SharedState` clone,
/// which the deeply-nested handler paths that move cursors don't have in scope.
pub async fn cursor_follow_loop(
    state: SharedState,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<(ClientId, BufferId, DeferredToken)>,
) {
    while let Some((client_id, buffer_id, token)) = rx.recv().await {
        let key = (client_id, buffer_id);
        let (blame_epoch, hl_epoch, symbol_path_pushes) = {
            let mut s = state.lock().await;
            let blame = s.blame_follow.contains(&key).then(|| {
                let gen = s.blame_follow_gen.entry(key).or_insert(0);
                *gen += 1;
                *gen
            });
            // An active search owns the highlight layer (the enable handler refuses too);
            // don't fight it from the follow side even if the unfollow is still in flight.
            let hl = (s.symbol_highlight_follow.contains(&key) && !s.searches.contains_key(&key))
                .then(|| {
                    let epoch = next_symbol_hl_epoch();
                    s.symbol_highlight_gen.insert(key, epoch);
                    epoch
                });
            // Resolved inline, not spawned: the breadcrumb is a lookup in the cached outline, so
            // there's nothing to debounce and no round-trip to supersede. Recomputed from the
            // *current* cursor rather than anything carried on the event, so a burst of moves
            // collapses onto the latest position by construction.
            let crumbs = collect_symbol_path_pushes(&mut s, key.1);
            (blame, hl, crumbs)
        };
        // The token rides into whatever this arms, so the work stays counted until the debounced
        // task actually finishes — or returns early, superseded. Nothing armed means it drops here
        // and the server is quiet again.
        if let Some(epoch) = blame_epoch {
            spawn_blame_refresh(state.clone(), key.0, key.1, epoch, token.clone());
        }
        if let Some(epoch) = hl_epoch {
            spawn_symbol_highlight_refresh(state.clone(), key.0, key.1, epoch, token.clone());
        }
        for (sender, notif) in symbol_path_pushes {
            let _ = sender.send(notif).await;
        }
    }
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
    require_viewport_mut(&mut s, params.viewport_id, client_id)?.diff_view = params.enabled;
    let buffer_id = s.focused_buffer(&s.viewports[&params.viewport_id]);
    // Refresh hunks so the first diff frame is accurate; clearing the view leaves them as-is
    // (harmless — nothing renders them).
    if params.enabled {
        recompute_diff_hunks_if_viewed(&mut s, buffer_id);
    }
    let window = render_viewport(&s, params.viewport_id, SneakLabels::Shown);
    Ok(ViewportWindowResult { window })
}

/// Jump the cursor to the start of the next/previous changed region (hunk). Works whether or not
/// the diff view is on, so it recomputes the buffer's hunks fresh — the call is user-initiated and
/// infrequent, so a one-off `git diff` is fine, and it keeps navigation correct even when edits
/// happened with the view off (which skips the per-edit recompute). Returns the (possibly
/// unchanged) cursor and whether it moved.
///
/// **In a conflicted file the targets are the conflict blocks instead.** There are no hunks there
/// to step between (no baseline to diff against), and the blocks are what the same gesture is
/// asking for. The client keeps one pair of keys either way.
pub async fn git_navigate_hunk(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: GitNavigateHunkParams,
) -> Result<GitNavigateHunkResult, RpcError> {
    let client_id = ctx.client_id;
    let s = state.lock().await;
    if !s.buffers.contains_key(&params.buffer_id) {
        return Err(RpcError::buffer_not_found(params.buffer_id));
    }
    let key = (client_id, params.buffer_id);
    let current = s.cursors.get(&key).copied().unwrap_or_default();
    let anchors = buffer_change_anchors(&s, params.buffer_id);
    // The field's extent. `c` is a *target* motion, so a hunk outside the element the cursor is in
    // is not a destination — the same rule `d` follows. In an ordinary editor view the field is the
    // whole buffer and this filters nothing; it matters wherever a view windows part of a file.
    let (field_first, field_last) = {
        let scope = s.motion_scope(client_id, params.buffer_id)?;
        (scope.first_line(), scope.last_line())
    };
    let anchors: Vec<u32> = anchors
        .into_iter()
        .filter(|&a| a >= field_first && a <= field_last)
        .collect();
    finish_hunk_navigation(s, client_id, params, current, anchors).await
}

/// The lines a buffer's own changes start on — what `c`/`Alt-c` step between in it, and what a
/// view whose element windows the buffer without a diff of its own steps through
/// `view/navigate_change`.
///
/// Three sources for "the next thing worth stepping to", picked by what the buffer *is*. Same keys,
/// same gesture, better answer — the pattern the conflict branch established.
pub fn buffer_change_anchors(s: &ServerState, buffer_id: BufferId) -> Vec<u32> {
    let conflicts = buffer_conflicts(s, buffer_id);
    if let Some(generated) = s.doc_of(buffer_id).generated.as_ref() {
        // A generated patch: its own change blocks, already in buffer-line coordinates.
        // Deliberately the blocks and not the hunks — a hunk opens with the context lines that
        // make it readable, so stopping at its start would land several lines above anything that
        // actually changed, and a hunk holding two separate edits would only stop once.
        generated
            .index
            .files
            .iter()
            .flat_map(|f| f.changes.iter().map(|c| c.start_line))
            .collect()
    } else if !conflicts.is_empty() {
        // In a conflicted file the hunks are the wrong targets — there are none, because a
        // conflicted path has no baseline to diff against — and the things worth stepping between
        // are the conflict blocks.
        conflicts.iter().map(|r| r.start_line).collect()
    } else {
        // Diff against the cached baselines — cheap (no repo I/O) and correct regardless of whether
        // a viewport is currently driving the per-edit recompute. Resolved through
        // `effective_baseline` so `c` steps between exactly the changes the gutter drew.
        //
        // While the comparison is the index the anchors are the union of the HEAD and index diffs,
        // so navigation reaches every change the combined view can show — including a region
        // reverted back to HEAD's content but staged differently (in the index diff only). Under a
        // pinned baseline there is no second layer to union in: a revision puts the same content in
        // both blobs, and the saved file has no HEAD side at all.
        let buf = s.doc_of(buffer_id);
        let baseline = s.git_baseline.get(&buffer_id);
        // A pending baseline offers no anchors — the gutter they would step between hasn't been
        // drawn yet either, and the push that draws it arrives a moment later.
        let effective =
            baseline.and_then(|b| crate::git::effective_baseline(b, buf.disk_blob.as_deref()));
        let unstaged = crate::git::diff_hunks(effective.as_ref().and_then(|e| e.blob), &buf.text);
        let head = if effective.as_ref().is_some_and(|e| e.pinned) {
            Vec::new()
        } else {
            let head_blob = baseline
                .and_then(|b| b.content())
                .and_then(|c| c.blob.as_deref());
            crate::git::diff_hunks(head_blob, &buf.text)
        };
        let mut anchors: Vec<u32> = head
            .iter()
            .chain(unstaged.iter())
            .map(|h| h.anchor_line)
            .collect();
        anchors.sort_unstable();
        anchors.dedup();
        anchors
    }
}

/// The step itself, over the anchors already scoped to the field.
async fn finish_hunk_navigation(
    mut s: tokio::sync::MutexGuard<'_, ServerState>,
    client_id: ClientId,
    params: GitNavigateHunkParams,
    current: CursorState,
    anchors: Vec<u32>,
) -> Result<GitNavigateHunkResult, RpcError> {
    let key = (client_id, params.buffer_id);
    // Walk `count` hunks in `direction`. An over-large count **refuses** rather than landing on the
    // last/first change: the count names which hunk, and there isn't one. At `count == 1` the two
    // readings coincide, so this changes nothing for a bare `c`.
    let skip = (params.count.max(1) - 1) as usize;
    let target = match params.direction {
        HunkDirection::Next => anchors
            .iter()
            .filter(|&&a| a > params.from_line)
            .nth(skip)
            .copied(),
        HunkDirection::Prev => anchors
            .iter()
            .rev()
            .filter(|&&a| a < params.from_line)
            .nth(skip)
            .copied(),
    };

    let Some(target_line) = target else {
        let response = wrap_for_response(&s, client_id, params.buffer_id, current);
        return Ok(GitNavigateHunkResult {
            cursor: response,
            moved: false,
        });
    };

    let buf = s.doc_of(params.buffer_id);
    let position = motion::clamp_position(
        buf,
        LogicalPosition {
            line: target_line,
            col: 0,
        },
    );
    let result = CursorState {
        position,
        // Extend keeps the existing anchor (grow the selection to the hunk); otherwise collapse to
        // a point at the landing line.
        anchor: if params.extend {
            current.anchor
        } else {
            position
        },
        match_bracket: None,
        jumplist_position: None,
    };
    set_cursor(&mut s, key, result);
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

/// What a stage/unstage issued from a patch buffer addresses.
///
/// The answers are kept apart because they refuse *differently*, and the refusal is what the user
/// reads: **only [`Self::NotAPatch`] may fall through** to the ordinary file apply, whose "no
/// baseline here" outcome the client words as *not in a git repository*. Reaching that from inside
/// a repo's own patch — which is what a single `Option` here used to do for a cursor parked on the
/// header — tells the user something false about their workspace.
enum PatchApply {
    /// An ordinary file buffer: the only thing the fall-through is for.
    NotAPatch,
    /// A commit's diff. It is history: there is nothing in it to stage, unstage or revert, at any
    /// cursor position — which is a different statement from [`Self::Nothing`]'s "not *here*".
    History,
    /// The working-changes view, but the cursor addresses nothing stageable — a context line, or a
    /// delta with no content of its own.
    Nothing,
    /// The file still on disk, and the worktree lines the cursor's change block covers.
    Lines {
        abs_path: std::path::PathBuf,
        /// 0-based worktree lines the block occupies. Empty for a pure removal, which holds no
        /// line of its own — `anchor` locates it instead.
        lines: Vec<u32>,
        /// 0-based worktree line a pure removal sits above.
        anchor: u32,
    },
    /// The file is **gone** from the worktree. There is no buffer to run the ordinary apply
    /// through (opening one would fail on the missing path), and no sub-unit to address either:
    /// the whole deletion is the change, so the index write is done directly.
    Deletion {
        workdir: std::path::PathBuf,
        rel: String,
    },
}

/// Resolve a stage/unstage aimed at the **patch buffer itself**.
///
/// The fallback, not the main road. A patch's hunks window real files, so the cursor is normally in
/// one of those files and the ordinary apply handles it; what reaches here is a cursor that has not
/// entered an element, or an element windowing the generated text because there is no file to
/// window (a deletion, a binary swap). Those are resolved through the patch's own line index, which
/// is the only thing that can name them.
///
/// Only the **working-changes** view; see [`PatchApply`] for why the ways of resolving nothing are
/// distinguished rather than collapsed.
async fn resolve_patch_apply_target(
    state: &SharedState,
    client_id: ClientId,
    buffer_id: BufferId,
) -> Result<PatchApply, RpcError> {
    let s = state.lock().await;
    let Some(doc) = s.try_doc_of(buffer_id) else {
        return Ok(PatchApply::NotAPatch);
    };
    let (Some(generated), Some(source)) = (doc.generated.as_ref(), doc.virtual_source.as_ref())
    else {
        return Ok(PatchApply::NotAPatch);
    };
    if source.target.what != aether_protocol::git::ShowTarget::WorkingChanges {
        return Ok(PatchApply::History);
    }
    let workdir = std::path::PathBuf::from(&source.target.repo_id);
    let line = s
        .cursors
        .get(&(client_id, buffer_id))
        .map(|c| c.position.line)
        .unwrap_or_default();
    let Some(Some(info)) = generated.index.lines.get(line as usize).copied() else {
        return Ok(PatchApply::Nothing);
    };
    let Some(file) = generated.index.files.get(info.file as usize) else {
        return Ok(PatchApply::Nothing);
    };
    // A deletion decides before the block does: every line of it is a removal, so there is nothing
    // to narrow to, and the path it names has nothing behind it on disk.
    if file.status == crate::patch::PatchFileStatus::Deleted {
        let Some(rel) = file.old_path.clone().or_else(|| file.new_path.clone()) else {
            return Ok(PatchApply::Nothing);
        };
        return Ok(PatchApply::Deletion { workdir, rel });
    }
    let Some(path) = file.new_path.as_deref() else {
        return Ok(PatchApply::Nothing);
    };
    // The block the cursor is in — the unit staging acts on, and the same one `c` steps between.
    let Some(block) = file
        .changes
        .iter()
        .find(|c| (c.start_line..c.end_line).contains(&line))
    else {
        return Ok(PatchApply::Nothing);
    };
    let lines: Vec<u32> = (block.start_line..block.end_line)
        .filter_map(|i| generated.index.lines.get(i as usize).copied().flatten())
        .filter_map(|i| i.new_lineno)
        .map(|n| n.saturating_sub(1))
        .collect();
    // A pure removal sits above the next surviving line; fall back to the file's first line.
    let anchor = (block.end_line..file.end_line)
        .filter_map(|i| generated.index.lines.get(i as usize).copied().flatten())
        .find_map(|i| i.new_lineno)
        .map_or(0, |n| n.saturating_sub(1));
    Ok(PatchApply::Lines {
        abs_path: workdir.join(path),
        lines,
        anchor,
    })
}

/// Report `status` against the patch buffer the user is looking at, echoing its cursor unmoved.
/// The shape every refusal issued from the patch view answers in.
async fn patch_outcome(
    state: &SharedState,
    client_id: ClientId,
    patch_buffer: BufferId,
    status: ApplyHunkStatus,
) -> GitApplyHunkResult {
    let s = state.lock().await;
    let cursor = s
        .cursors
        .get(&(client_id, patch_buffer))
        .copied()
        .unwrap_or_default();
    GitApplyHunkResult {
        cursor: wrap_for_response(&s, client_id, patch_buffer, cursor),
        status,
    }
}

/// Stage or unstage a whole deleted file from the patch view — `git add` / `git reset` on a path
/// with nothing behind it.
///
/// Written straight to the index, like [`crate::git::write_index_blob`] and for the same reason:
/// there is no buffer here to route through, and a deletion has no content for a filter to clean.
/// The whole file is the unit — every line of the delta is a removal, so there is no smaller thing
/// the cursor could have meant.
async fn apply_deletion_via_patch(
    state: &SharedState,
    client_id: ClientId,
    patch_buffer: BufferId,
    action: HunkAction,
    workdir: std::path::PathBuf,
    rel: String,
) -> Result<GitApplyHunkResult, RpcError> {
    let repo = workdir.clone();
    let status =
        tokio::task::spawn_blocking(move || crate::git::apply_deletion(&workdir, &rel, action))
            .await
            .map_err(|e| RpcError::internal(format!("git apply_hunk: {e}")))?;

    // Same rebuild as the ordinary patch-view apply: the text doesn't move (the file is gone
    // either way), but its stage tag does.
    if matches!(status, ApplyHunkStatus::Staged | ApplyHunkStatus::Unstaged) {
        refresh_working_changes_views(state, &std::iter::once(repo).collect()).await;
    }
    Ok(patch_outcome(state, client_id, patch_buffer, status).await)
}

/// Open the file the block belongs to, seat this client's cursor on the block, and run the ordinary
/// apply against it — then rebuild the patch, whose stage tags are the only thing the action moved.
async fn apply_hunk_via_patch(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: GitApplyHunkParams,
    abs_path: std::path::PathBuf,
    lines: Vec<u32>,
    anchor: u32,
) -> Result<GitApplyHunkResult, RpcError> {
    let client_id = ctx.client_id;
    let patch_buffer = params.buffer_id;

    // Transient: staging from the diff shouldn't leave a trail of buffers you never asked to open.
    let file = Box::pin(view_open(
        state,
        ctx,
        ViewOpenParams {
            absolute_path: Some(abs_path.to_string_lossy().into_owned()),
            transient: Some(true),
            ..Default::default()
        },
    ))
    .await?;

    {
        let mut s = state.lock().await;
        // `ApplyScope::File` ignores the cursor, so only the block scope needs seating. A pure
        // removal has no line of its own — address the line it sits above, which is what
        // `WholeHunkAt` resolves a deletion by.
        let (lo, hi) = match (lines.first(), lines.last()) {
            (Some(&lo), Some(&hi)) => (lo, hi),
            _ => (anchor, anchor),
        };
        let doc = s.doc_of(file.buffer_id);
        let position = motion::clamp_position(doc, LogicalPosition { line: hi, col: 0 });
        let anchor = motion::clamp_position(doc, LogicalPosition { line: lo, col: 0 });
        set_cursor(
            &mut s,
            (client_id, file.buffer_id),
            CursorState {
                position,
                anchor,
                match_bracket: None,
                jumplist_position: None,
            },
        );
    }

    let applied = Box::pin(git_apply_hunk(
        state,
        ctx,
        GitApplyHunkParams {
            buffer_id: file.buffer_id,
            ..params
        },
    ))
    .await?;

    // The rebuild this patch needs — its text is unchanged (staging doesn't move HEAD) but its
    // stage tags are — already happened inside the delegated call: an index write refreshes every
    // working-changes view of the repo, and this buffer is one of them.
    //
    // Report against the patch buffer the user is actually looking at, not the file we borrowed.
    Ok(patch_outcome(state, client_id, patch_buffer, applied.status).await)
}

/// Where a cursor sat in a patch, in terms a *rebuilt* patch can still find.
///
/// Buffer lines don't survive a rebuild — every change above the cursor moves them — so a live view
/// that re-seated by line number would slide the reader around whenever an unrelated file was
/// saved. The file and the source line within it do survive, and they are what the user was
/// actually looking at.
struct PatchAnchor {
    /// Repo-relative path, from whichever side the cursor's line belongs to.
    path: String,
    /// The line number on that side, 1-based as libgit2 reports it. `None` on chrome and on a
    /// placeholder line, which pin to the file and nothing finer.
    lineno: Option<u32>,
    /// Which side, so a `-` line doesn't re-seat onto the `+` line that replaced it.
    side: Option<PatchLine>,
    col: u32,
}

/// Read a client's cursor as a [`PatchAnchor`]. `None` when it sits on chrome that belongs to no
/// file — the metadata block, the closing message — which has nothing to anchor to.
fn patch_anchor(
    generated: &crate::patch::GeneratedPatch,
    cursor: CursorState,
) -> Option<PatchAnchor> {
    let line = cursor.position.line;
    let info = (*generated.index.lines.get(line as usize)?)?;
    let file = generated.index.files.get(info.file as usize)?;
    let (path, lineno) = match info.side {
        Some(PatchLine::Removed) => (file.old_path.as_ref(), info.old_lineno),
        _ => (file.new_path.as_ref(), info.new_lineno),
    };
    Some(PatchAnchor {
        // A deletion has no new path and an addition no old one; either way the file has one of
        // the two, and that is the name it is listed under.
        path: path
            .or(file.new_path.as_ref())
            .or(file.old_path.as_ref())?
            .clone(),
        lineno,
        side: info.side,
        col: cursor.position.col,
    })
}

/// Find the buffer line the anchor names in a freshly generated patch: the same source line of the
/// same file, else the nearest surviving line of it, else the file's first line. `None` once the
/// file has no changes left at all — the caller falls back to clamping.
fn reseat_patch_anchor(
    generated: &crate::patch::GeneratedPatch,
    anchor: &PatchAnchor,
) -> Option<u32> {
    let idx = generated.index.files.iter().position(|f| {
        f.new_path.as_deref() == Some(anchor.path.as_str())
            || f.old_path.as_deref() == Some(anchor.path.as_str())
    })?;
    let file = &generated.index.files[idx];
    let Some(want) = anchor.lineno else {
        return Some(file.start_line);
    };
    let mut best: Option<(u32, u32)> = None;
    for line in file.start_line..file.end_line {
        let Some(Some(info)) = generated.index.lines.get(line as usize).copied() else {
            continue;
        };
        let got = match anchor.side {
            Some(PatchLine::Removed) => info.old_lineno,
            _ => info.new_lineno,
        };
        let Some(got) = got else { continue };
        let distance = got.abs_diff(want);
        if best.is_none_or(|(d, _)| distance < d) {
            best = Some((distance, line));
        }
    }
    Some(best.map_or(file.start_line, |(_, line)| line))
}

/// Rebuild a working-changes buffer in place and push the new content to whoever is viewing it.
///
/// A no-op when the rebuilt patch is the same one — which is most of the time, since this runs off
/// every write under the repo. Both halves of "the same" are checked: the text covers content
/// changes, and the stage tags cover staging, which moves nothing else.
async fn regenerate_patch_buffer(state: &SharedState, buffer_id: BufferId) {
    let (workdir, baseline) = {
        let s = state.lock().await;
        let Some(source) = s
            .try_doc_of(buffer_id)
            .and_then(|d| d.virtual_source.as_ref())
        else {
            return;
        };
        let workdir = std::path::PathBuf::from(&source.target.repo_id);
        let baseline = s.git_baseline_choices.get(&workdir).cloned();
        (workdir, baseline)
    };
    let Ok(content) = tokio::task::spawn_blocking({
        let baseline = baseline.clone();
        move || crate::git::show_working_changes(&workdir, baseline.as_ref())
    })
    .await
    else {
        return;
    };
    let Ok(content) = content else { return };

    let pushes = {
        let mut s = state.lock().await;
        let Some(doc_id) = s.buffers.get(&buffer_id).map(|b| b.document) else {
            return;
        };
        let Some(doc) = s.documents.get(&doc_id) else {
            return;
        };
        let unchanged = doc.text == content.text
            && doc.generated.as_ref().map(|g| &g.decorations.stage)
                == content.generated.as_ref().map(|g| &g.decorations.stage);
        // The baseline is the third thing that can move. It usually moves the text with it, but not
        // always — re-baselining onto `HEAD` with nothing staged produces the identical patch — and
        // the status bar's token has to follow either way.
        let baseline_moved =
            s.virtual_git_status.get(&buffer_id).map(|st| &st.baseline) != Some(&baseline);
        if unchanged && !baseline_moved {
            return;
        }
        if let Some(status) = s.virtual_git_status.get_mut(&buffer_id) {
            status.baseline = baseline;
        }
        let Some(doc) = s.documents.get(&doc_id) else {
            return;
        };
        // Read every viewer's place *before* the swap, in the coordinates the old patch used.
        // A selection is left to the clamp below: re-seating one endpoint of a range across a
        // rebuild would mean something the user didn't select.
        let anchors: Vec<(ClientId, PatchAnchor)> = s
            .cursors
            .iter()
            .filter(|((_, b), cur)| *b == buffer_id && cur.is_point())
            .filter_map(|((c, _), cur)| {
                let anchor = doc.generated.as_ref().and_then(|g| patch_anchor(g, *cur))?;
                Some((*c, anchor))
            })
            .collect();

        s.replace_generated(buffer_id, &content.text, content.generated);
        for (client, anchor) in anchors {
            let Some(line) = s
                .doc_of(buffer_id)
                .generated
                .as_ref()
                .and_then(|g| reseat_patch_anchor(g, &anchor))
            else {
                continue; // the file has no changes left; the clamp below decides
            };
            let position = motion::clamp_position(
                s.doc_of(buffer_id),
                LogicalPosition {
                    line,
                    col: anchor.col,
                },
            );
            set_cursor(
                &mut s,
                (client, buffer_id),
                CursorState {
                    position,
                    anchor: position,
                    match_bracket: None,
                    jumplist_position: None,
                },
            );
        }
        // Everything not re-seated above — selections, viewers of a sibling buffer on the same
        // document, a cursor whose file is gone from the diff — still has to land inside the new
        // text.
        clamp_doc_cursors(&mut s, buffer_id);
        refresh_viewport_ranges_for_buffer(&mut s, buffer_id);
        let mut pushes = collect_doc_lines_changed_pushes(&s, buffer_id);
        // The state push as well as the content one, exactly as a reload sends both. A client
        // takes its `revision` from `viewport/lines_changed` but its `saved_revision` only from
        // `buffer/state` — so content alone leaves the two apart, and the buffer reads as edited.
        pushes.extend(collect_buffer_state_pushes(&s, buffer_id));
        pushes
    };
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
}

/// The repos that currently have a working-changes view open, by workdir.
///
/// Nearly always empty, which is what makes [`refresh_working_changes_views`] free to call from
/// every path that moves a tree: with no view open there is nothing to rebuild and no diff to run.
pub(crate) fn working_changes_repos(
    s: &ServerState,
) -> std::collections::HashSet<std::path::PathBuf> {
    s.buffers
        .keys()
        .filter_map(|id| s.try_doc_of(*id).and_then(|d| d.virtual_source.as_ref()))
        .filter(|v| v.target.what == aether_protocol::git::ShowTarget::WorkingChanges)
        .map(|v| std::path::PathBuf::from(&v.target.repo_id))
        .collect()
}

/// Rebuild every open working-changes view of one of `workdirs` and push the result.
///
/// The working tree is the one [`aether_protocol::git::ShowTarget`] that *moves*, so a view of it
/// is live rather than a snapshot: a saved file, a stage, a commit — in the editor or in a terminal
/// — all change what the view is *of*, and a patch left on the old answer quietly lies about the
/// tree. Every path that moves a tree funnels through here rather than growing a rebuild of its
/// own, and the ones that don't move anything the view can see cost a hash lookup.
pub(crate) async fn refresh_working_changes_views(
    state: &SharedState,
    workdirs: &std::collections::HashSet<std::path::PathBuf>,
) {
    if workdirs.is_empty() {
        return;
    }
    let views: Vec<BufferId> = {
        let s = state.lock().await;
        let mut seen: std::collections::HashSet<crate::state::DocumentId> =
            std::collections::HashSet::new();
        s.buffers
            .iter()
            .filter(|(id, buf)| {
                seen.insert(buf.document)
                    && s.try_doc_of(**id)
                        .and_then(|d| d.virtual_source.as_ref())
                        .is_some_and(|v| {
                            v.target.what == aether_protocol::git::ShowTarget::WorkingChanges
                                && workdirs.contains(&std::path::PathBuf::from(&v.target.repo_id))
                        })
            })
            .map(|(id, _)| *id)
            .collect()
    };
    for id in views {
        regenerate_patch_buffer(state, id).await;
    }
}

/// Stage, unstage or revert the change under the cursor (bare cursor → the whole hunk it sits on)
/// or the selected lines (any wider selection, snapped to whole lines). The server resolves the
/// client's cursor/selection authoritatively — no positions in the params.
///
/// - **Stage** writes the region's unstaged change into the index (index ← buffer, resolved
///   against the index→buffer diff).
/// - **Unstage** pulls the region's staged change back out (index region ← HEAD, resolved against
///   the HEAD→index diff with the cursor/selection carried from buffer to index coordinates across
///   any unstaged edits — the region is addressed in buffer lines, and the two coordinate spaces
///   differ by exactly the unstaged edits).
///
///   Both require a non-dirty buffer: the index must never hold content that exists nowhere on
///   disk. A region with nothing facing the requested direction is `NoChange`, not an error.
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

    // A baseline the deferred loader hasn't got to yet is a cache miss, not an answer — resolve it
    // rather than refuse. Cheap no-op whenever one is already cached, which is every apply against
    // a file that has been open for more than a moment.
    ensure_git_baseline(state, buffer_id).await;

    // Staging *from the working-changes view*, for the case where the cursor is still on the patch
    // buffer itself.
    //
    // Usually it is not: a patch's hunks window the real files, so focus rebinds the view's buffer
    // to the file under the cursor and the client names *that* buffer here — whereupon this returns
    // `NotAPatch` and the ordinary apply below runs against the real file's baseline, with no patch
    // coordinates involved at all. That is the element-resolved path, and it is the normal one.
    //
    // What still arrives on the patch buffer: a cursor that has not entered an element yet, and
    // elements that window the generated text because there is no file to window — a deleted file,
    // a binary swap. Those resolve through the patch's own index, which is why it stays.
    //
    // Every arm but `NotAPatch` answers here. The fall-through below reads a *file's* baseline, and
    // a patch buffer has none — so letting one reach it reports `Unavailable`, which the client
    // words as "not in a git repository" about a repo whose own diff is on screen.
    let resolved = resolve_patch_apply_target(state, client_id, params.buffer_id).await?;
    // Revert is deliberately not offered from the working-changes view. Stage and unstage are index
    // writes and need no buffer; a revert is an *undoable edit*, so delegating it would put the undo
    // in a transient buffer the user never opened and can't see — and losing an undo is worse than
    // not offering the key. `Enter` opens the file, where reverting means what it says.
    //
    // A commit's diff is excluded: reverting there isn't unavailable-for-now, it's meaningless, and
    // pointing at the file wouldn't help.
    let working_changes = matches!(
        resolved,
        PatchApply::Nothing | PatchApply::Lines { .. } | PatchApply::Deletion { .. }
    );
    if params.action == HunkAction::Revert && working_changes {
        return Ok(patch_outcome(state, client_id, buffer_id, ApplyHunkStatus::NeedsFile).await);
    }
    match resolved {
        PatchApply::NotAPatch => {}
        // History has nothing to act on, wherever the cursor sits.
        PatchApply::History | PatchApply::Nothing => {
            return Ok(patch_outcome(state, client_id, buffer_id, ApplyHunkStatus::NoChange).await);
        }
        PatchApply::Lines {
            abs_path,
            lines,
            anchor,
        } => return apply_hunk_via_patch(state, ctx, params, abs_path, lines, anchor).await,
        PatchApply::Deletion { workdir, rel } => {
            return apply_deletion_via_patch(
                state,
                client_id,
                buffer_id,
                params.action,
                workdir,
                rel,
            )
            .await;
        }
    }

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
        .try_doc_of(buffer_id)
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
    let sel = match params.scope {
        // Every line, wherever the cursor is — the hunk rule read over the whole file, with the
        // action itself unchanged: stage takes everything unstaged, unstage everything staged.
        ApplyScope::File => crate::git::HunkSelection::Lines {
            lo: 0,
            hi: u32::MAX,
        },
        ApplyScope::Cursor if cursor.is_point() => {
            crate::git::HunkSelection::WholeHunkAt(cursor.position.line)
        }
        ApplyScope::Cursor => crate::git::HunkSelection::Lines {
            lo: cursor.anchor.line.min(cursor.position.line),
            hi: cursor.anchor.line.max(cursor.position.line),
        },
    };

    let Some(baseline) = s.git_baseline.get(&buffer_id) else {
        return Ok(outcome(&s, ApplyHunkStatus::Unavailable));
    };
    let Some(repo) = baseline.repo.clone() else {
        return Ok(outcome(&s, ApplyHunkStatus::Unavailable));
    };
    // Resolved above by `ensure_git_baseline`; still pending here means that load failed, which is
    // a genuine "can't tell" rather than "there is no repo".
    let Some(content) = baseline.content() else {
        return Ok(outcome(&s, ApplyHunkStatus::Unavailable));
    };
    let head_blob = content.blob.clone();
    let index_blob = content.index_blob.clone();
    // Index writes require the gutter to *be* about the index. Against a pinned revision both
    // blobs hold that commit's content, so staging would write the merge of it and the buffer into
    // the index — content the user never asked to stage — and there is no index relationship to
    // unstage out of either. Against the saved file there is no git content in the comparison at
    // all. Revert stays meaningful in both ("put this hunk back to how it was there") and falls
    // through — against the saved file that is "discard this hunk's unsaved edits".
    //
    // Keyed on `pinned` rather than on the resolved blob: an untracked file's *absent* baseline is
    // still the index's own answer about it, and staging it (a hunk-wise `git add` of the whole
    // thing) stays meaningful.
    let pinned = crate::git::effective_baseline(
        baseline,
        s.try_doc_of(buffer_id).and_then(|d| d.disk_blob.as_deref()),
    )
    // The content is resolved by here (`ensure_git_baseline`, and the guard above), so this is
    // `Some`; a pending baseline is not pinned by anything either way.
    .is_some_and(|e| e.pinned);
    if pinned && !matches!(params.action, HunkAction::Revert) {
        return Ok(outcome(&s, ApplyHunkStatus::NotAgainstHead));
    }
    // A conflicted path is a different world: there is no stage-0 index entry and no baseline, so
    // nothing here has its usual meaning. Whole-file staging becomes "mark resolved" — the identical
    // git command with the identical intent, "make the index match my file" — and everything else
    // is refused rather than quietly doing something the key doesn't name. Unstaging is refused at
    // *both* scopes: with no stage-0 entry there is nothing to pull out, and the inverse gesture
    // (putting the conflict back) is `Space g d`, not a key that says "unstage".
    if crate::git::path_has_conflict(&repo.workdir, &repo.rel_path) {
        if !matches!(
            (params.action, params.scope),
            (HunkAction::Stage, ApplyScope::File)
        ) {
            return Ok(outcome(&s, ApplyHunkStatus::Conflicted));
        }
        // `git add` writes what's on disk, so an unsaved buffer would mark a *different* file
        // resolved. Refused rather than saved for them, as everywhere else.
        if dirty {
            return Ok(outcome(&s, ApplyHunkStatus::DirtyBuffer));
        }
        // Checked from the buffer text, not the cached scan: this is the last gate before the
        // markers could reach a commit, and it must be true *now*.
        if !crate::git::conflict_regions(&s.doc_of(buffer_id).text).is_empty() {
            return Ok(outcome(&s, ApplyHunkStatus::MarkersRemain));
        }
        // Through the CLI, not `write_index_blob`: this *is* `git add`, and going through git runs
        // the user's clean filters and clears the conflict stages the way every other tool expects.
        let rel = path_string(&repo.rel_path);
        drop(s);
        let out = crate::git_cli::run(&repo.workdir, &["add", "--", &rel])
            .await
            .map_err(|e| RpcError::internal(format!("running git add: {e}")))?;
        let mut s = state.lock().await;
        if !out.success() {
            return Ok(outcome(&s, ApplyHunkStatus::Unavailable));
        }
        // The file is no longer conflicted: reload the baseline so the gutter, the status bar and
        // the conflict decoration all stop describing a state that has ended.
        let pushes = refresh_git_for_buffer(&mut s, buffer_id);
        let result = outcome(&s, ApplyHunkStatus::Resolved);
        drop(s);
        for (sender, notif) in pushes {
            let _ = sender.send(notif).await;
        }
        return Ok(result);
    }

    match params.action {
        action @ (HunkAction::Stage | HunkAction::Unstage) => {
            if dirty {
                return Ok(outcome(&s, ApplyHunkStatus::DirtyBuffer));
            }
            let index_bytes = index_blob.as_deref().unwrap_or(b"");
            let (merged, status) = match action {
                // Stage: index ← buffer over the region. An untracked file's empty index baseline
                // makes this the hunk-wise `git add`.
                HunkAction::Stage => {
                    match crate::git::merge_selected(
                        index_bytes,
                        buffer_text.as_bytes(),
                        &sel,
                        true,
                    ) {
                        Some(content) => (content, ApplyHunkStatus::Staged),
                        // Nothing unstaged in the region — already staged, or not a change at all.
                        None => return Ok(outcome(&s, ApplyHunkStatus::NoChange)),
                    }
                }
                // Unstage: index region ← HEAD. The region is addressed in *buffer* lines, so carry
                // it into index lines across the unstaged (index→buffer) diff first — the two
                // coordinate spaces differ by exactly the edits that haven't been staged.
                _ => {
                    let unstaged =
                        crate::git::diff_hunks(Some(index_bytes), &s.doc_of(buffer_id).text);
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
            // The index moved, so any working-changes view of this repo is now showing the wrong
            // stage tags. This is the single site for it: staging *from* that view delegates here
            // for the file it resolved, so the view is rebuilt whichever end the key was pressed
            // at — and a view open in another shell or workspace follows too.
            refresh_working_changes_views(state, &std::iter::once(repo.workdir).collect()).await;
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
            let buf = s.doc_of(buffer_id);
            let was_dirty = buf.dirty;
            let old_len = buf.text.len_chars();
            let cursors_before = document_cursor_snapshot(&s, buffer_id);
            let mut buf_mut = s.editable_doc(buffer_id)?;
            buf_mut.apply_edit(0, old_len, &new_text, EditKindTag::Revert, cursors_before);

            // Clamp every cursor on the buffer into the reverted rope.
            clamp_doc_cursors(&mut s, buffer_id);
            s.clear_motion_history_for_buffer(buffer_id);
            s.clear_tree_selection_history_for_buffer(buffer_id);
            s.clear_virtual_col_for_buffer(buffer_id);

            let mut search_summary_pushes = promote_transient(&mut s, buffer_id);
            search_summary_pushes.extend(refresh_searches_for_buffer(&mut s, buffer_id));
            // Also recomputes the cached hunks, so the pushed gutter markers are post-revert.
            refresh_viewport_ranges_for_buffer(&mut s, buffer_id);
            notify_lsp_change(&mut s, buffer_id);

            let pushes: PendingPushes = collect_doc_lines_changed_pushes(&s, buffer_id);
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

/// Take a side in the conflict block(s) the cursor or selection addresses (`Space g <` / `>` /
/// `=`, keyed to the marker glyphs rather than to ours/theirs — the side's *position* is invariant
/// across merge, rebase and cherry-pick where its meaning is not).
///
/// Deliberately an ordinary buffer edit, applied through the same whole-document replacement path as
/// `git/apply_hunk`'s revert and tagged [`EditKindTag::Resolve`] so each take is one undo step.
/// Nothing touches the index: the file is still conflicted as far as git is concerned until it is
/// saved and marked resolved, which is a separate decision made with a separate key.
pub async fn git_resolve_conflict(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: GitResolveConflictParams,
) -> Result<GitResolveConflictResult, RpcError> {
    let client_id = ctx.client_id;
    let buffer_id = params.buffer_id;
    let mut s = state.lock().await;

    let outcome = |s: &ServerState, status: ResolveConflictStatus| -> GitResolveConflictResult {
        let cursor = s
            .cursors
            .get(&(client_id, buffer_id))
            .copied()
            .unwrap_or_default();
        GitResolveConflictResult {
            cursor: wrap_for_response(s, client_id, buffer_id, cursor),
            status,
            resolved: 0,
            remaining: buffer_conflicts(s, buffer_id).len() as u32,
        }
    };

    if s.try_doc_of(buffer_id).is_none() {
        return Err(RpcError::buffer_not_found(buffer_id));
    }
    let cursor = s
        .cursors
        .get(&(client_id, buffer_id))
        .copied()
        .unwrap_or_default();
    // `git/apply_hunk`'s addressing rule: a bare cursor takes the block it's in, a wider selection
    // takes every block it touches — so select-all then `Space g >` is "all of the bottom side".
    let (lo, hi) = if cursor.is_point() {
        (cursor.position.line, cursor.position.line)
    } else {
        (
            cursor.anchor.line.min(cursor.position.line),
            cursor.anchor.line.max(cursor.position.line),
        )
    };
    let selected: Vec<&crate::git::ConflictRegion> = buffer_conflicts(&s, buffer_id)
        .iter()
        .filter(|r| r.overlaps(lo, hi))
        .collect();
    if selected.is_empty() {
        return Ok(outcome(&s, ResolveConflictStatus::NoConflict));
    }
    let resolved = selected.len() as u32;
    // Land the cursor where the resolved content now starts. The block it was sitting in has just
    // collapsed, so anything else — clamping, or holding the old line — puts it somewhere arbitrary.
    let landing = selected[0].start_line;

    let doc = s.doc_of(buffer_id);
    let new_text = crate::git::resolve_conflicts(&doc.text, &selected, params.side);
    drop(selected);

    let was_dirty = doc.dirty;
    let old_len = doc.text.len_chars();
    let cursors_before = document_cursor_snapshot(&s, buffer_id);
    let mut buf_mut = s.editable_doc(buffer_id)?;
    buf_mut.apply_edit(0, old_len, &new_text, EditKindTag::Resolve, cursors_before);

    clamp_doc_cursors(&mut s, buffer_id);
    s.clear_motion_history_for_buffer(buffer_id);
    s.clear_tree_selection_history_for_buffer(buffer_id);
    s.clear_virtual_col_for_buffer(buffer_id);
    let landing = motion::clamp_position(
        s.doc_of(buffer_id),
        LogicalPosition {
            line: landing,
            col: 0,
        },
    );
    set_cursor(
        &mut s,
        (client_id, buffer_id),
        CursorState {
            position: landing,
            anchor: landing,
            match_bracket: None,
            jumplist_position: None,
        },
    );

    let mut search_summary_pushes = promote_transient(&mut s, buffer_id);
    search_summary_pushes.extend(refresh_searches_for_buffer(&mut s, buffer_id));
    refresh_viewport_ranges_for_buffer(&mut s, buffer_id);
    // Explicitly, *not* relying on the refresh above: that rescan is gated on the buffer having a
    // viewport (the per-edit work is only worth doing for something on screen), and the count this
    // call reports — "how many are left" — has to be true whether or not anyone is looking.
    recompute_conflicts(&mut s, buffer_id);
    notify_lsp_change(&mut s, buffer_id);

    let pushes: PendingPushes = collect_doc_lines_changed_pushes(&s, buffer_id);
    let picker_pushes = maybe_refresh_dirty(&mut s, buffer_id, was_dirty);

    let mut result = outcome(&s, ResolveConflictStatus::Resolved);
    result.resolved = resolved;
    drop(s);
    for (sender, notif) in pushes
        .into_iter()
        .chain(search_summary_pushes)
        .chain(picker_pushes)
    {
        let _ = sender.send(notif).await;
    }
    Ok(result)
}

#[cfg(test)]
mod find_nearest_git_change_tests {
    use super::*;
    use aether_protocol::viewport::DiffStage;

    fn cand(rel: &str, hunk_index: u32, line: u32) -> picker_state::GitChangeCandidate {
        picker_state::GitChangeCandidate::new(
            0,
            rel.into(),
            format!("/p/{rel}"),
            hunk_index,
            line,
            DiffStage::Unstaged,
            1,
            0,
            vec!["x".into()],
        )
    }

    #[test]
    fn picks_first_hunk_at_or_after_the_cursor_in_the_active_file() {
        // a.rs has hunks at lines 4 and 20; b.rs at line 2.
        let cands = [cand("a.rs", 0, 4), cand("a.rs", 1, 20), cand("b.rs", 0, 2)];
        // Cursor on a.rs line 10 → the next hunk at or after it (line 20, index 1).
        assert_eq!(find_nearest_git_change(&cands, "/p/a.rs", 10), Some(1));
        // Cursor exactly on a hunk line is inclusive.
        assert_eq!(find_nearest_git_change(&cands, "/p/a.rs", 4), Some(0));
        // Cursor before every hunk → the file's first hunk.
        assert_eq!(find_nearest_git_change(&cands, "/p/a.rs", 0), Some(0));
    }

    #[test]
    fn falls_back_to_the_files_last_hunk_past_the_end() {
        let cands = [cand("a.rs", 0, 4), cand("a.rs", 1, 20), cand("b.rs", 0, 2)];
        // Cursor past every a.rs hunk → that file's last hunk (index 1), not b.rs.
        assert_eq!(find_nearest_git_change(&cands, "/p/a.rs", 99), Some(1));
    }

    #[test]
    fn no_match_when_the_active_file_has_no_changes() {
        let cands = [cand("a.rs", 0, 4), cand("b.rs", 0, 2)];
        // The active file isn't in the change set → None (the picker opens at the top, not on an
        // unrelated file).
        assert_eq!(find_nearest_git_change(&cands, "/p/untouched.rs", 0), None);
    }
}
