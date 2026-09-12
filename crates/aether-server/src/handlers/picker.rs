//! `picker/*` — the unified picker overlay: view, query, select, group expansion.

use super::*;

/// Which of the three per-kind pickers a view or a dormant row belongs to, or `None` for one that
/// belongs to no picker at all.
///
/// One classification, consulted by all three builders, so a thing can never be in two lists: a
/// shell's transcript and an agent's conversation name themselves through their
/// [`crate::state::VirtualTarget`], and a **buffer** is a document's own view — a file, a scratch,
/// or a file as of a revision.
///
/// A commit's patch and the working changes are deliberately in no list. Their text is generated
/// to *be* the view and what you read in them lives in the buffers their elements window, so they
/// are not something the user opened by name; you reach them again by git command, the log picker
/// or history. The test is structural ([`crate::state::View::is_composed`]) and names no git target.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RowKind {
    Buffer,
    Shell,
    Agent,
}

impl RowKind {
    /// A live view's list, by what it presents and how it is composed.
    fn of_view(view: &crate::state::View, doc: &Document) -> Option<RowKind> {
        match doc.virtual_source.as_ref().map(|v| &v.target) {
            Some(crate::state::VirtualTarget::Shell { .. }) => Some(RowKind::Shell),
            Some(crate::state::VirtualTarget::Agent { .. }) => Some(RowKind::Agent),
            _ => (!view.is_composed()).then_some(RowKind::Buffer),
        }
    }

    /// A dormant row's list. There is no view to inspect, so it is decided from what the row would
    /// materialise as: a file and a scratch are documents; a virtual key is a buffer only when it
    /// names a **document** — a file at a revision, never a commit or the working changes.
    fn of_dormant(source: &crate::state::DormantSource) -> Option<RowKind> {
        match source {
            crate::state::DormantSource::Shell { .. } => Some(RowKind::Shell),
            crate::state::DormantSource::Agent { .. } => Some(RowKind::Agent),
            crate::state::DormantSource::File(_) | crate::state::DormantSource::Scratch { .. } => {
                Some(RowKind::Buffer)
            }
            crate::state::DormantSource::Virtual { key } => {
                (!crate::state::VirtualTarget::key_is_composed(key)).then_some(RowKind::Buffer)
            }
        }
    }
}

/// The views and dormant rows of `client_id`'s active workspace that belong in `kind`'s picker, in
/// the order every one of the three lists them: **MRU first**, then the kept-but-unvisited
/// leftovers by id, then the session's dormant rows.
///
/// One walk shared by the three builders. Recency is the only ordering — status is a badge, so a
/// run starting or a turn ending re-paints a row and never moves it.
fn picker_rows(
    s: &ServerState,
    client_id: ClientId,
    kind: RowKind,
) -> (Vec<ViewId>, Vec<&crate::state::DormantView>) {
    let Some(workspace) = s.active_workspace(client_id) else {
        return (Vec::new(), Vec::new());
    };
    let workspace_name = workspace.id.clone();
    let belongs =
        |id: &BufferId| s.buffer_workspaces.get(id).map(|s| s.as_str()) == Some(&workspace_name);
    // A field of a view is not a view: a shell's input has no row of its own, and a row that
    // opened one would present half a shell. See `Document::internal`.
    let listable = |view: &crate::state::View| -> Option<RowKind> {
        let id = view.presenting;
        if !belongs(&id) {
            return None;
        }
        let doc = s.try_doc_of(id)?;
        if doc.internal || !s.buffers.contains_key(&id) {
            return None;
        }
        RowKind::of_view(view, doc)
    };

    let mut views = Vec::new();
    let mut seen: std::collections::HashSet<ViewId> = std::collections::HashSet::new();
    for &view_id in &workspace.mru_views {
        let Some(view) = s.try_view(view_id) else {
            continue;
        };
        if listable(view) != Some(kind) {
            continue;
        }
        views.push(view_id);
        seen.insert(view_id);
    }
    // Views nothing has landed on yet — a file bound into a review and then kept — sorted by id
    // so the sweep is deterministic. Previews you never visited stay out: a preview is somewhere
    // you looked, and the MRU has every one of those.
    let mut leftovers: Vec<ViewId> = s
        .views
        .iter()
        .filter(|(view_id, view)| !seen.contains(view_id) && listable(view) == Some(kind))
        .filter(|(_, view)| !view.transient)
        .map(|(view_id, _)| *view_id)
        .collect();
    leftovers.sort_unstable();
    views.extend(leftovers);

    let live_paths: std::collections::HashSet<&std::path::Path> = s
        .buffers
        .iter()
        .filter(|(id, _)| belongs(id))
        .filter_map(|(_, b)| s.documents.get(&b.document)?.canonical_path.as_deref())
        .collect();
    let dormant: Vec<&crate::state::DormantView> = workspace
        .dormant_views
        .iter()
        .filter(|d| RowKind::of_dormant(&d.source) == Some(kind))
        // A dormant *file* whose path is already open as a live buffer shouldn't double-show; a
        // dormant scratch (no path) can never collide, so it always shows.
        .filter(|d| d.path().is_none_or(|p| !live_paths.contains(p)))
        .collect();
    (views, dormant)
}

/// Build the buffers-picker candidate list for `client_id`: every *buffer* of the client's active
/// workspace — a file, a scratch, or a file as of a revision — most-recently-used first, then the
/// kept-but-unvisited ones, then the session's dormant rows. `(scratch N)` placeholder display for
/// buffers without a path. Empty when the client has no active workspace (the picker shouldn't be
/// reachable without one, but the lookup stays defensive).
///
/// Shells and conversations are deliberately absent: they have pickers of their own, whose rows
/// answer questions a path cannot. So are a commit's patch and the working changes, which are not
/// buffers at all — see [`RowKind`].
fn build_buffer_candidates(
    s: &ServerState,
    client_id: ClientId,
) -> Vec<picker_state::BufferCandidate> {
    let Some(workspace) = s.active_workspace(client_id) else {
        return Vec::new();
    };
    let roots = workspace.paths.clone();
    let (views, dormant) = picker_rows(s, client_id, RowKind::Buffer);
    let mut out = Vec::with_capacity(views.len() + dormant.len());
    for view_id in views {
        let view = &s.views[&view_id];
        let id = view.presenting;
        out.push(buffer_candidate(
            &s.buffers[&id],
            s.doc_of(id),
            view_id,
            view,
            &roots,
        ));
    }
    for d in dormant {
        out.push(dormant_candidate(d, &roots));
    }
    out
}

/// Build the shells-picker candidate list: one row per shell view, live or dormant.
///
/// Row data comes off the [`crate::shell::Transcript`] — its title, where the next command would
/// run, and how the last one went.
fn build_shell_candidates(
    s: &ServerState,
    client_id: ClientId,
) -> Vec<picker_state::ShellCandidate> {
    let (views, dormant) = picker_rows(s, client_id, RowKind::Shell);
    let mut out = Vec::with_capacity(views.len() + dormant.len());
    for view_id in views {
        let Some(t) = s
            .try_view(view_id)
            .and_then(|v| s.try_doc_of(v.presenting))
            .and_then(|d| d.transcript())
        else {
            continue;
        };
        // The last *finished* run: while one is in flight, the badge says so and these two still
        // describe the run before it, which is what you were looking for when you opened the list.
        let finished = t.runs.iter().rev().find(|r| !r.is_running());
        // Shortened `$HOME` → `~`, the shell view's own [`crate::shell::display_path`]: the row and
        // the run box must not disagree about how a directory is written, and the haystack has to
        // hold the string the row shows or the fuzzy highlight would land off the text.
        let cwd = crate::shell::display_path(&t.cwd);
        let last_command = t.runs.last().map(|r| r.command.clone());
        out.push(picker_state::ShellCandidate {
            view_id,
            haystack: shell_haystack(&t.title, &cwd, last_command.as_deref()),
            title: t.title.clone(),
            cwd,
            last_command,
            running: t.active().is_some(),
            exit: finished.and_then(|r| match r.status {
                aether_protocol::shell::RunStatus::Exited { code } => Some(code),
                _ => None,
            }),
            elapsed_ms: finished.and_then(|r| r.elapsed_ms),
            dormant: false,
        });
    }
    for d in dormant {
        let crate::state::DormantSource::Shell { number } = &d.source else {
            continue;
        };
        // A dormant shell's snapshot holds its directory and its runs, and reading it is a disk hit
        // per row — so a row that has not been opened says its name and nothing else. Selecting it
        // materialises the shell, and the refresh that follows fills the row in.
        let title = format!("Shell {number}");
        out.push(picker_state::ShellCandidate {
            view_id: d.view,
            haystack: shell_haystack(&title, "", None),
            title,
            cwd: String::new(),
            last_command: None,
            running: false,
            exit: None,
            elapsed_ms: None,
            dormant: true,
        });
    }
    out
}

/// Build the agents-picker candidate list: one row per conversation, live or dormant.
///
/// Row data comes off the [`crate::agent::Conversation`] — which agent is behind it, whether a turn
/// is in flight, whether it is blocked on a permission request, and the last thing the user said.
fn build_agent_candidates(
    s: &ServerState,
    client_id: ClientId,
) -> Vec<picker_state::AgentCandidate> {
    let (views, dormant) = picker_rows(s, client_id, RowKind::Agent);
    let mut out = Vec::with_capacity(views.len() + dormant.len());
    for view_id in views {
        let Some(c) = s
            .try_view(view_id)
            .and_then(|v| s.try_doc_of(v.presenting))
            .and_then(|d| d.conversation())
        else {
            continue;
        };
        // Blocked beats running: a turn waiting on an answer is not making progress, and saying
        // "thinking" about one would hide the only row that needs the user.
        let state = if c.pending_permission().is_some() {
            AgentRowState::AwaitingPermission
        } else if let Some(turn) = &c.turn {
            AgentRowState::Thinking {
                activity: turn.activity.clone(),
            }
        } else if c.handle.is_none() {
            AgentRowState::Disconnected
        } else {
            AgentRowState::Idle
        };
        let last_prompt = c
            .blocks
            .iter()
            .rev()
            .find(|b| matches!(b.kind, crate::agent::BlockKind::UserMessage))
            .and_then(|b| s.try_doc_of(b.buffer))
            .map(|d| d.text.to_string())
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        out.push(picker_state::AgentCandidate {
            view_id,
            haystack: agent_haystack(&c.title, c.agent.name, last_prompt.as_deref()),
            title: c.title.clone(),
            agent: c.agent.name.to_string(),
            state,
            last_prompt,
            dormant: false,
        });
    }
    for d in dormant {
        let crate::state::DormantSource::Agent { number } = &d.source else {
            continue;
        };
        // As with a dormant shell: the snapshot holds the agent's name and its blocks, and reading
        // one per row is a disk hit the list does not need. There is certainly no subprocess.
        let title = format!("Agent {number}");
        out.push(picker_state::AgentCandidate {
            view_id: d.view,
            haystack: agent_haystack(&title, "", None),
            title,
            agent: String::new(),
            state: AgentRowState::Disconnected,
            last_prompt: None,
            dormant: true,
        });
    }
    out
}

/// The shells picker's fuzzy haystack: `"{title}  {cwd}  {last_command}"`, empty parts elided.
///
/// A **wire contract** — `PickerItem::Shell::match_indices` are char offsets into this string, so a
/// shell rendering the three fields apart splits the offsets by the same joins. Two spaces between
/// the parts, so a query cannot fuzzily bridge two of them on a single space.
fn shell_haystack(title: &str, cwd: &str, last_command: Option<&str>) -> String {
    join_haystack([title, cwd, last_command.unwrap_or("")])
}

/// The agents picker's fuzzy haystack: `"{title}  {agent}  {last_prompt}"`, empty parts elided.
/// A wire contract for the reason [`shell_haystack`] is.
fn agent_haystack(title: &str, agent: &str, last_prompt: Option<&str>) -> String {
    join_haystack([title, agent, last_prompt.unwrap_or("")])
}

fn join_haystack(parts: [&str; 3]) -> String {
    parts
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("  ")
}

/// Picker candidate for a dormant (session-restored, not-yet-loaded) buffer: it carries the
/// reserved id as its picker identity, but has no live buffer behind it. The row is
/// indistinguishable from a live one — selecting it materializes the real buffer, so the
/// distinction never changes what the user can do. A file's display/path is derived from its path;
/// a dormant scratch shows `(scratch N)` and has no opener path.
fn dormant_candidate(
    d: &crate::state::DormantView,
    roots: &[std::path::PathBuf],
) -> picker_state::BufferCandidate {
    let (display, path) = match &d.source {
        crate::state::DormantSource::File(p) => (
            crate::workspace_index::workspace_relative_display(p, roots)
                .unwrap_or_else(|| p.display().to_string()),
            crate::workspace_index::workspace_relative_parts(p, roots),
        ),
        crate::state::DormantSource::Scratch { number } => (format!("(scratch {number})"), None),
        crate::state::DormantSource::Shell { number } => (format!("Shell {number}"), None),
        crate::state::DormantSource::Agent { number } => (format!("Agent {number}"), None),
        // Named as the live view names itself, as far as the key allows: a revision is its short
        // hash and path (`abc1234:src/a.rs`) — the subject is generated with the content, which a
        // dormant entry hasn't paid for yet. Only a file at a revision reaches here (`RowKind::
        // of_dormant`); the other shapes stay written out so the naming is total rather than
        // leaning on the filter above.
        crate::state::DormantSource::Virtual { key } => (
            match crate::state::VirtualTarget::parse_key(key).and_then(|t| t.what().cloned()) {
                Some(aether_protocol::git::ShowTarget::WorkingChanges) => "Working changes".into(),
                Some(aether_protocol::git::ShowTarget::Commit { rev }) => {
                    rev.chars().take(7).collect()
                }
                Some(aether_protocol::git::ShowTarget::File { rev, path }) => {
                    format!("{}:{path}", rev.chars().take(7).collect::<String>())
                }
                None => key.clone(),
            },
            None,
        ),
    };
    // A dormant *file* row is Clean — its content lives safely on disk; closing it just forgets the
    // session entry. A dormant *scratch* exists only because unsaved content survived as a backup, so
    // it's inherently Unsaved: report that, both for an accurate dirty dot and so `Ctrl-d` routes
    // through the discard-confirm prompt instead of silently dropping the backup.
    let status = match &d.source {
        // A revision is Clean for the same reason a file is, and more so: it is read-only, so
        // there was never anything to save.
        // A shell's snapshot is safe on disk, and a shell is never unsaved work. Nor is a
        // conversation: it is a record of what was said, not a document you owe a save to.
        crate::state::DormantSource::File(_)
        | crate::state::DormantSource::Virtual { .. }
        | crate::state::DormantSource::Shell { .. }
        | crate::state::DormantSource::Agent { .. } => BufferDirtyState::Clean,
        crate::state::DormantSource::Scratch { .. } => BufferDirtyState::Unsaved,
    };
    picker_state::BufferCandidate {
        buffer_id: d.id,
        view_id: d.view,
        display,
        status,
        path,
        abs_path: match &d.source {
            crate::state::DormantSource::File(p) => Some(p.to_string_lossy().into_owned()),
            // No file to open behind either: a revision is regenerated, and a scratch is content
            // with nowhere on disk to live.
            crate::state::DormantSource::Scratch { .. }
            | crate::state::DormantSource::Virtual { .. }
            | crate::state::DormantSource::Shell { .. }
            | crate::state::DormantSource::Agent { .. } => None,
        },
        transient: false,
    }
}

fn buffer_candidate(
    buf: &Buffer,
    doc: &Document,
    view_id: ViewId,
    view: &crate::state::View,
    roots: &[std::path::PathBuf],
) -> picker_state::BufferCandidate {
    let display = match (doc.canonical_path.as_deref(), &doc.virtual_source) {
        (Some(p), _) => crate::workspace_index::workspace_relative_display(p, roots)
            .unwrap_or_else(|| p.display().to_string()),
        // A virtual buffer is pathless but named: show the revision title rather than calling a
        // commit's diff "(scratch 3)".
        (None, Some(v)) => v.title.clone(),
        (None, None) => format!(
            "(scratch {})",
            buf.scratch_number.map(u64::from).unwrap_or(buf.id)
        ),
    };
    // The (root index, relative path) the client needs for an opener URL — `None` for scratch
    // buffers and files outside every root (display still falls back to the absolute path above).
    let path = doc
        .canonical_path
        .as_deref()
        .and_then(|p| crate::workspace_index::workspace_relative_parts(p, roots));
    picker_state::BufferCandidate {
        buffer_id: buf.id,
        view_id,
        display,
        status: buffer_dirty_state(doc),
        path,
        abs_path: doc
            .canonical_path
            .as_deref()
            .map(|p| p.to_string_lossy().into_owned()),
        transient: view.transient,
    }
}

/// Map a buffer's save/disk flags to the picker's [`BufferDirtyState`], highest precedence first:
/// removed on disk → changed on disk → unsaved local edits → clean. Mirrors the editor status
/// bar's dot so the picker and the status line always agree.
fn buffer_dirty_state(buf: &Document) -> BufferDirtyState {
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

/// Rebuild candidates for every subscribed picker whose list a git *ref* operation just changed —
/// the stash picker after a push/apply/pop/drop, the branch picker after a delete. Re-ranks under
/// the existing query, so the user's filter and their place in the list survive.
///
/// Deferred once, on the reasoning that checkout closes its picker so nothing could observe a
/// stale list. That was true of checkout and wrong in general: dropping a stash (or deleting a
/// branch) leaves the picker *open*, and it kept showing the row that had just been removed —
/// which the next keystroke would then act on.
///
/// Cheap when nothing is open: a scan over `pickers` and an early return. Rebuilt for every client
/// with one subscribed, not just the one that acted, since `refs/stash` is repo-wide.
pub(crate) fn refresh_git_ref_pickers(s: &mut ServerState, kind: PickerKind) -> PendingPushes {
    let client_ids: Vec<ClientId> = s
        .pickers
        .iter()
        .filter_map(|((c, k), p)| (*k == kind && p.subscribed.is_some()).then_some(*c))
        .collect();
    let mut pushes = Vec::new();
    for client_id in client_ids {
        // Re-resolve from the repo the picker's own rows name, not from the active buffer: the
        // buffer may have moved (or be a preview that closed) since the picker opened, and a
        // refresh must not silently repoint the list at a different repo.
        let workdir = {
            let picker = match s.pickers.get(&(client_id, kind)) {
                Some(p) => p,
                None => continue,
            };
            match &picker.candidates {
                picker_state::PickerCandidates::GitStash(v) => v.first().map(|c| c.repo_id.clone()),
                // Re-resolve from the **main** worktree's own path where the rows name one, not
                // from their `repo_id`. Every row carries the tree the picker was opened over,
                // which since the merge is a tree a removal may just have deleted — and re-reading
                // a deleted directory yields an empty list, so the refresh that should show "the
                // tree is gone, here is what's left" would empty the picker instead. The main tree
                // is in the same family, so the listing is identical, and it is the one tree that
                // cannot be removed. Absent only for a bare or `--separate-git-dir` repo, where
                // `worktree::list` omits it and the rows' own repo is the best available.
                picker_state::PickerCandidates::GitBranches(v) => v
                    .iter()
                    .find_map(|c| c.row.checkout.as_ref().filter(|k| k.is_main))
                    .map(|k| k.path.clone())
                    .or_else(|| v.first().map(|c| c.repo_id.clone())),
                _ => None,
            }
        };
        // An empty list has no repo to re-read: nothing to refresh, and the next open resolves
        // afresh anyway.
        let Some(workdir) = workdir.map(std::path::PathBuf::from) else {
            continue;
        };
        let repo_id = workdir.to_string_lossy().into_owned();
        let new_candidates = match kind {
            PickerKind::GitStash => picker_state::PickerCandidates::GitStash(
                crate::git::list_stashes(&workdir)
                    .into_iter()
                    .map(|row| picker_state::GitStashCandidate {
                        repo_id: repo_id.clone(),
                        row,
                    })
                    .collect(),
            ),
            // Rebuilt after a mutation because removal is the one row action that leaves the
            // picker *open* — no confirm dialog to close it, no switch to follow — so without this
            // the tree you just removed stays listed and pressing the key again acts on a row that
            // isn't there. Creation is the same in reverse: `Ctrl-o` deliberately stays put, and
            // the new worktree marker appearing on the row is the feedback that it worked.
            PickerKind::GitBranches => picker_state::PickerCandidates::GitBranches(
                crate::git::branch_picker_rows(&workdir)
                    .into_iter()
                    .map(|row| picker_state::GitBranchCandidate {
                        repo_id: repo_id.clone(),
                        row,
                    })
                    .collect(),
            ),
            _ => continue,
        };
        let ServerState {
            pickers,
            matcher,
            clients,
            ..
        } = &mut *s;
        let Some(picker) = pickers.get_mut(&(client_id, kind)) else {
            continue;
        };
        picker.candidates = new_candidates;
        picker.rerank(matcher);
        // The list can only shrink here, so a window sitting past the new end would render empty.
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

/// Rebuild and re-push **all three** view-listing pickers — buffers, shells, agents.
///
/// What the ordinary "something about the open set changed" call sites want: a view closing, a
/// workspace activating or a file being renamed can touch any of the three lists, and asking which
/// at each of twenty call sites is how a list goes stale. Cheap when nothing is open: a HashMap
/// scan per kind and an early return.
///
/// The two places that know exactly which list moved call the per-kind function instead — a run
/// starting or finishing ([`refresh_shell_pickers`]) and an agent event
/// ([`refresh_agent_pickers`]) — because those fire often and touch one list each.
pub(crate) fn refresh_view_pickers(s: &mut ServerState) -> PendingPushes {
    let mut pushes = refresh_kind_pickers(s, PickerKind::Buffers);
    pushes.extend(refresh_kind_pickers(s, PickerKind::Shells));
    pushes.extend(refresh_kind_pickers(s, PickerKind::Agents));
    pushes
}

/// Rebuild and re-push every subscribed shells picker — a run started, finished or was cancelled,
/// which changes a badge and never an order.
pub(crate) fn refresh_shell_pickers(s: &mut ServerState) -> PendingPushes {
    refresh_kind_pickers(s, PickerKind::Shells)
}

/// Rebuild and re-push every subscribed agents picker — a turn started or ended, a permission was
/// raised or answered, or a handle was dropped.
pub(crate) fn refresh_agent_pickers(s: &mut ServerState) -> PendingPushes {
    refresh_kind_pickers(s, PickerKind::Agents)
}

/// Rebuild candidates for every subscribed picker of one view-listing `kind`, re-rank under the
/// existing query, and collect the resulting `picker/update` pushes. Caller sends them after
/// dropping the lock.
fn refresh_kind_pickers(s: &mut ServerState, kind: PickerKind) -> PendingPushes {
    // Collect client_ids with a *subscribed* picker of this kind. Skip the rest — they may still
    // have persisted state from a prior session, but they're not waiting for pushes.
    let client_ids: Vec<ClientId> = s
        .pickers
        .iter()
        .filter_map(|((c, k), p)| (*k == kind && p.subscribed.is_some()).then_some(*c))
        .collect();
    let mut pushes = Vec::new();
    for client_id in client_ids {
        let new_candidates = build_view_candidates(s, client_id, kind);
        let ServerState {
            pickers,
            matcher,
            clients,
            ..
        } = &mut *s;
        let Some(picker) = pickers.get_mut(&(client_id, kind)) else {
            continue;
        };
        picker.candidates = new_candidates;
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

/// The candidate set for one of the three view-listing kinds. Panics-free for any other kind: it
/// answers an empty buffers list, which no caller asks for.
fn build_view_candidates(
    s: &ServerState,
    client_id: ClientId,
    kind: PickerKind,
) -> picker_state::PickerCandidates {
    match kind {
        PickerKind::Shells => {
            picker_state::PickerCandidates::Shells(build_shell_candidates(s, client_id))
        }
        PickerKind::Agents => {
            picker_state::PickerCandidates::Agents(build_agent_candidates(s, client_id))
        }
        _ => picker_state::PickerCandidates::Buffers(build_buffer_candidates(s, client_id)),
    }
}

/// Rebuild and re-push every subscribed `Workspaces` picker. Called after a workspace is created,
/// renamed, or deleted (by any client) so an open chooser elsewhere reflects the new set live.
/// Mirrors [`refresh_view_pickers`]; the candidate list is a disk read, so callers must have
/// already written the config change before invoking this.
pub(crate) fn refresh_workspace_pickers(s: &mut ServerState) -> PendingPushes {
    let client_ids: Vec<ClientId> = s
        .pickers
        .iter()
        .filter_map(|((c, k), p)| {
            (*k == PickerKind::Workspaces && p.subscribed.is_some()).then_some(*c)
        })
        .collect();
    if client_ids.is_empty() {
        return Vec::new();
    }
    // One disk read of the workspaces directory, shared by every subscribed picker.
    let names = match s
        .workspaces_dir()
        .and_then(|d| crate::config::list_workspace_names_in(&d))
    {
        Ok(n) => n,
        Err(_) => return Vec::new(), // can't enumerate — leave the pickers as they are
    };
    let mut pushes = Vec::new();
    for client_id in client_ids {
        let candidates = workspace_candidates(s, &names);
        let ServerState {
            pickers,
            matcher,
            clients,
            ..
        } = &mut *s;
        let Some(picker) = pickers.get_mut(&(client_id, PickerKind::Workspaces)) else {
            continue;
        };
        picker.candidates = picker_state::PickerCandidates::Workspaces(candidates);
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

/// Drop `workspace_id`'s project pins once no client has it active, reaping the servers that only
/// the pins were keeping alive.
///
/// A pin belongs to the workspace, not to a client, so this fires only when the *last* client
/// leaves: switching away in one window while another still shows the workspace must not tear its
/// servers down. A pinned server that has buffers open survives either way — `unpin_workspace`
/// hands it back to the ordinary reap-on-last-buffer lifetime.
pub fn unpin_workspace_if_unused(s: &mut ServerState, workspace_id: &str) -> PendingPushes {
    let still_active = s
        .clients
        .values()
        .any(|c| c.active_workspace.as_deref() == Some(workspace_id));
    if still_active || s.lsp.unpin_workspace(workspace_id).is_empty() {
        return Vec::new();
    }
    refresh_lsp_server_pickers(s)
}

/// Rebuild and re-push every subscribed `LspServers` picker. Called whenever a server's status
/// changes (from `crate::lsp::manager`) so the open dialog's health glyphs update live — e.g.
/// `◐ → ●` as a restart completes. Mirrors [`refresh_view_pickers`].
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
        let Some((workspace, roots)) = s
            .active_workspace(client_id)
            .map(|p| (p.id.clone(), p.paths.clone()))
        else {
            continue;
        };
        let new_candidates = build_lsp_server_candidates(s, &workspace, &roots);
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

/// Rebuild every *open* workspace-diagnostics picker (`Space Alt-d`) from the current
/// `path_diagnostics` and push the update — so it live-updates as servers push diagnostics (while
/// rust-analyzer indexes the workspace, or after a `cargo check`) instead of showing only the
/// snapshot from when it opened. Called on every diagnostics push; a no-op (empty, no rebuild) when
/// no client has the picker open. Mirrors [`refresh_lsp_server_pickers`].
pub fn refresh_workspace_diagnostics_pickers(s: &mut ServerState) -> PendingPushes {
    let client_ids: Vec<ClientId> = s
        .pickers
        .iter()
        .filter_map(|((c, k), p)| {
            (*k == PickerKind::DiagnosticsWorkspace && p.subscribed.is_some()).then_some(*c)
        })
        .collect();
    let mut pushes = Vec::new();
    for client_id in client_ids {
        let new_candidates = build_workspace_diagnostic_candidates(s, client_id);
        let ServerState {
            pickers,
            matcher,
            clients,
            ..
        } = &mut *s;
        let Some(picker) = pickers.get_mut(&(client_id, PickerKind::DiagnosticsWorkspace)) else {
            continue;
        };
        picker.candidates = picker_state::PickerCandidates::Diagnostics(new_candidates);
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

/// Tell the *other* clients standing in `actor`'s context that its jumplist just changed, so any
/// open Jumplist picker can re-view. Called after a capture or a clear.
///
/// Unlike [`refresh_view_pickers`] this pushes no rows — see
/// [`aether_protocol::jumplist::JumplistChanged`] for why a capture's change of *shape* has to go
/// through `picker/view` rather than a `picker/update`. Scoped two ways: to clients in the same
/// context (another context's picker lists its own list, untouched), and to those with the picker
/// actually open (`subscribed`, which `picker/hide` clears). `actor` is excluded — a capture already
/// re-frames its own client's picker, and a clear is a Normal-mode chord with none open.
pub(crate) fn jumplist_changed_pushes(s: &ServerState, actor: ClientId) -> PendingPushes {
    let Some(context) = s
        .clients
        .get(&actor)
        .and_then(|c| c.active_workspace.as_deref())
    else {
        return Vec::new();
    };
    s.pickers
        .iter()
        .filter(|((c, kind), p)| {
            *kind == PickerKind::Jumplist && p.subscribed.is_some() && *c != actor
        })
        .filter_map(|((c, _), _)| {
            let session = s.clients.get(c)?;
            (session.active_workspace.as_deref() == Some(context)).then(|| {
                (
                    session.outbound.clone(),
                    Notification {
                        jsonrpc: JsonRpc,
                        method: aether_protocol::jumplist::JumplistChanged::NAME.into(),
                        params: serde_json::to_value(
                            aether_protocol::jumplist::JumplistChangedParams {},
                        )
                        .expect("infallible"),
                    },
                )
            })
        })
        .collect()
}

/// If `buffer_id`'s dirty flag changed across the just-completed mutation, collect picker
/// refresh pushes. Caller captures `was_dirty` before the mutation; this reads the post-
/// mutation value and decides. No-op (no allocation, no rerank) when dirty didn't change —
/// the typical hot path during a typing burst.
pub fn maybe_refresh_dirty(
    s: &mut ServerState,
    buffer_id: BufferId,
    was_dirty: bool,
) -> PendingPushes {
    let now_dirty = s.try_doc_of(buffer_id).map(|b| b.dirty).unwrap_or(false);
    if now_dirty == was_dirty {
        Vec::new()
    } else {
        refresh_view_pickers(s)
    }
}

/// Resolve and validate an Explorer *anchor* — the committed directory the query peeks relative
/// to. Canonicalizes, enforces the workspace boundary, requires a directory, and computes the
/// (in-workspace) parent for Alt-h ascent. Errors propagate to the client (a bad `directory_path`
/// is a real navigation error), unlike a bad *peek* path, which just lists nothing.
fn resolve_explorer_anchor(
    raw: &std::path::Path,
    workspace_paths: &[std::path::PathBuf],
) -> Result<picker_state::ExplorerAnchorInfo, RpcError> {
    let in_workspace = |p: &std::path::Path| {
        workspace_paths
            .iter()
            .any(|r| p == r.as_path() || p.starts_with(r))
    };
    let canonical = std::fs::canonicalize(raw)
        .map_err(|e| RpcError::invalid_path(format!("canonicalizing {}: {e}", raw.display())))?;
    if !in_workspace(&canonical) {
        return Err(RpcError::invalid_path(format!(
            "{} is outside the workspace's access boundary",
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
        .and_then(|p| in_workspace(p).then(|| p.display().to_string()));
    Ok(picker_state::ExplorerAnchorInfo {
        path: canonical.display().to_string(),
        parent,
    })
}

/// Build the Explorer listing for `query`, relative to the committed `anchor`. The query's path
/// part (everything up to the last `/`) selects the directory to list — `anchor/<path_part>`, the
/// "peek"; the filter part (after the last `/`) is applied later by the prefix matcher. Returns
/// the listing plus `peek_missing`: true when the path part doesn't resolve to an in-workspace
/// directory (mid-typing a not-yet-created path — the "+ Create" case), in which case the listing
/// is empty and `path` still names the intended target so a file-watcher refresh can't bind it to
/// an unrelated real directory. The client reads `peek_missing` to decide whether `dir/` offers
/// "+ Create directory" (the listing shows the *contents*, so it can't tell on its own).
fn build_explorer_peek(
    anchor: &std::path::Path,
    query: &str,
    workspace_paths: &[std::path::PathBuf],
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
        .and_then(|c| build_explorer_candidates_for_canonical(&c, workspace_paths, filters).ok())
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
/// workspace-boundary rules as `directory_list`. Used by `picker_view` for `PickerKind::Explorer`.
/// The anchor is the requested path *or* the persisted anchor (when the client omitted the path
/// on a scroll/resume) *or* the first workspace root (first ever open); the listing peeks from it
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
    // One lock pass: workspace roots + the explorer's committed anchor + its current query (which
    // drives the peek). On `reset` the query is being wiped, so peek from the anchor itself.
    let (workspace_paths, existing_anchor, query) = {
        let s = state.lock().await;
        let picker = s.pickers.get(&(client_id, PickerKind::Explorer));
        let existing_anchor = picker.and_then(|p| p.explorer_anchor.clone());
        let query = if reset {
            String::new()
        } else {
            picker.map(|p| p.query.clone()).unwrap_or_default()
        };
        (
            s.active_workspace_or_err(client_id)?.paths.clone(),
            existing_anchor,
            query,
        )
    };
    let anchor_raw: std::path::PathBuf = if let Some(p) = requested {
        std::path::PathBuf::from(p)
    } else if let Some(a) = &existing_anchor {
        std::path::PathBuf::from(&a.path)
    } else {
        workspace_paths
            .first()
            .cloned()
            .ok_or_else(|| RpcError::invalid_path("no workspace paths configured"))?
    };
    let anchor = resolve_explorer_anchor(&anchor_raw, &workspace_paths)?;
    let (listing, peek_missing) = build_explorer_peek(
        std::path::Path::new(&anchor.path),
        &query,
        &workspace_paths,
        filters,
    );
    Ok((listing, anchor, peek_missing))
}

/// Build the Roots-mode candidate list — one row per workspace root, sorted by basename. The
/// matcher haystack is the basename alone (the disambiguator the client renders is purely
/// presentational).
async fn build_explorer_roots(
    state: &SharedState,
    client_id: ClientId,
) -> Result<Vec<picker_state::RootCandidate>, RpcError> {
    let s = state.lock().await;
    let workspace = s.active_workspace_or_err(client_id)?;
    let mut out: Vec<picker_state::RootCandidate> = workspace
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
    workspace_paths: &[std::path::PathBuf],
    filters: &aether_protocol::picker::PickerFilters,
) -> Result<picker_state::ExplorerCandidates, RpcError> {
    let in_workspace = |p: &std::path::Path| -> bool {
        workspace_paths
            .iter()
            .any(|root| p == root.as_path() || p.starts_with(root))
    };
    if !in_workspace(canonical) {
        return Err(RpcError::invalid_path(format!(
            "{} is outside the workspace's access boundary",
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
        if in_workspace(p) {
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
        // Hide untracked entries (and directories whose aggregated status is untracked — a wholly
        // new subtree). Composes with `changed_only`: changed + tracked-only, or all-tracked alone.
        if filters.hide_untracked && git_status == Some(aether_protocol::git::GitStatus::Untracked)
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

/// A directory's immediate children as `(name, is_dir)`, dirs first then files, alphabetical within
/// each — the order every path-completing surface presents.
///
/// Deliberately *not* [`build_explorer_candidates_for_canonical`], which this used to borrow: that
/// one runs [`crate::git::dir_statuses`] — a `Repository::discover` walk-up plus a whole-repo status
/// pass with untracked *and* ignored included — and [`DirectoryEntry`] has nowhere to put the
/// answer, so every byte of it was thrown away. Harmless-looking until the completing field is an
/// absolute path: `~/` is the most-typed prefix there, and a home directory is very often itself a
/// repo, which would mean a full status pass per keystroke.
fn read_dir_sorted(canonical: &std::path::Path) -> Result<Vec<DirectoryEntry>, RpcError> {
    let mut entries: Vec<DirectoryEntry> = Vec::new();
    for ent in std::fs::read_dir(canonical).map_err(RpcError::file_io)? {
        let Ok(ent) = ent else { continue };
        let Ok(name) = ent.file_name().into_string() else {
            continue;
        };
        let is_dir = ent.file_type().map(|t| t.is_dir()).unwrap_or(false);
        entries.push(DirectoryEntry { name, is_dir });
    }
    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.cmp(&b.name),
    });
    Ok(entries)
}

/// One-shot directory listing for the client's path-completing fields. No per-client state — just
/// canonicalize, read, return; the client filters and cycles locally.
///
/// Two modes, per [`DirectoryListParams::unrestricted`]: bounded (the default — inside the active
/// workspace, same rule as the Explorer picker) and unrestricted (anywhere readable, no workspace
/// required). The bounded mode keeps the Explorer's `parent` convention — `None` at or above a
/// root, so a client can't walk out. Unrestricted has no boundary to report a parent relative to,
/// and its callers don't use the field.
pub async fn directory_list(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: DirectoryListParams,
) -> Result<DirectoryListResult, RpcError> {
    if params.unrestricted {
        // `~/…` is resolved here rather than client-side: the client core compiles to wasm for the
        // browser shell, where there is no `$HOME` to expand against.
        let raw = crate::config::expand_home(std::path::Path::new(&params.path));
        // The same rule (and the same reason) as `workspace/open_path`: a relative path would
        // resolve against the *daemon's* working directory, which is meaningless to the user — so
        // the completions would come from one place while the commit refused the path outright.
        if !raw.is_absolute() {
            return Err(RpcError::invalid_path(format!(
                "path must be absolute: {}",
                raw.display()
            )));
        }
        let canonical = std::fs::canonicalize(&raw).map_err(|e| {
            RpcError::invalid_path(format!("canonicalizing {}: {e}", raw.display()))
        })?;
        if !canonical.is_dir() {
            return Err(RpcError::invalid_path(format!(
                "{} is not a directory",
                canonical.display()
            )));
        }
        return Ok(DirectoryListResult {
            path: canonical.display().to_string(),
            parent: None,
            entries: read_dir_sorted(&canonical)?,
        });
    }

    let workspace_paths = {
        let s = state.lock().await;
        s.active_workspace_or_err(ctx.client_id)?.paths.clone()
    };
    let raw = std::path::PathBuf::from(&params.path);
    let canonical = std::fs::canonicalize(&raw)
        .map_err(|e| RpcError::invalid_path(format!("canonicalizing {}: {e}", raw.display())))?;
    let in_workspace = |p: &std::path::Path| -> bool {
        workspace_paths
            .iter()
            .any(|root| p == root.as_path() || p.starts_with(root))
    };
    if !in_workspace(&canonical) {
        return Err(RpcError::invalid_path(format!(
            "{} is outside the workspace's access boundary",
            canonical.display()
        )));
    }
    if !canonical.is_dir() {
        return Err(RpcError::invalid_path(format!(
            "{} is not a directory",
            canonical.display()
        )));
    }
    Ok(DirectoryListResult {
        path: canonical.display().to_string(),
        parent: canonical
            .parent()
            .filter(|p| in_workspace(p))
            .map(|p| p.display().to_string()),
        entries: read_dir_sorted(&canonical)?,
    })
}

/// Create a directory (and any missing intermediates), enforcing the workspace boundary first so
/// a `../escape/newdir` request can't produce dirs above the workspace root. Returns the
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
        if !s
            .active_workspace_or_err(ctx.client_id)?
            .contains(&resolved)
        {
            return Err(RpcError::invalid_path(format!(
                "{} is outside the workspace's access boundary",
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
        // Each picker's workspace may differ — re-fetch per client. Skip silently if the client
        // somehow lost its active workspace between subscribe and refresh.
        let Some(workspace_paths) = s.active_workspace(client_id).map(|p| p.paths.clone()) else {
            continue;
        };
        let new_candidates =
            match build_explorer_candidates_for_canonical(&path, &workspace_paths, &filters) {
                Ok(c) => c,
                Err(_) => continue, // dir removed or no longer in workspace; skip silently
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

/// Per-file Git status for the Files picker, aligned to `files` by index. Resolves each workspace
/// root's repo status once (one `statuses` per root), then looks each file up by its
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

/// The changes-picker rows for an open **patch** buffer: one per hunk, grouped by the file it came
/// from.
///
/// `Space c` means "the changes in this buffer" either way. In a patch that happens to span
/// several files, so the rows carry group headers where a single file's rows wouldn't — the
/// grouping is a consequence of the scope, not a different picker.
/// The **outline** of a composed view: a row per change, grouped by file, labelled by the enclosing
/// signature git records in the hunk header.
///
/// Built from [`view_outline`], which `o`/`Alt-o` and the status bar's breadcrumb also read — so the
/// three cannot disagree about what the stops are or what they are called. It used to resolve
/// document symbols for whichever file the cursor was in: an outline of one hunk's file, describing
/// nothing about the review.
///
/// The label is *where the change is*, not what it says — which is what makes this an outline and
/// not a second changes picker. Where git offers no signature (the top of a file, a non-code file)
/// the changed text stands in, because a row with no label at all cannot be picked out of a list.
fn build_outline_candidates(
    s: &ServerState,
    vp: &crate::state::Viewport,
    text: &ropey::Rope,
) -> Vec<crate::picker::GitChangeCandidate> {
    let view_buffer = s.view_of(vp).presenting;
    crate::handlers::viewport::view_outline(s, vp)
        .into_iter()
        .enumerate()
        .map(|(i, e)| {
            let label = if e.label.is_empty() {
                text.line(e.patch_line as usize)
                    .chunks()
                    .collect::<String>()
                    .trim()
                    .to_string()
            } else {
                e.label.clone()
            };
            // The element's own buffer and line, when it windows a real file — what everything
            // acting on this row actually wants.
            let file = s
                .view_of(vp)
                .elements
                .get(e.element as usize)
                .filter(|b| b.buffer_id != view_buffer)
                .map(|b| (e.element, b.buffer_id, e.line));
            // And the file itself, for a jumplist entry to keep: a working-tree file by its path,
            // a file at a revision by the key that re-materialises it.
            let durable = e.identity.clone().map(|identity| {
                let target = if identity.starts_with('/') {
                    crate::jumplist::JumplistTarget::File {
                        path_index: None,
                        relative_path: None,
                        abs_path: identity,
                    }
                } else {
                    crate::jumplist::JumplistTarget::View { key: identity }
                };
                (target, e.file_lines.start)
            });
            crate::picker::GitChangeCandidate::for_patch(
                crate::picker::PatchRowTarget {
                    view: vp.view_id,
                    file,
                    durable,
                    line_of: vec![e.patch_line],
                },
                e.file,
                i as u32,
                e.patch_line,
                0,
                0,
                vec![label],
            )
        })
        .collect()
}

fn build_patch_change_candidates(
    view_id: ViewId,
    generated: &crate::patch::GeneratedPatch,
    text: &ropey::Rope,
) -> Vec<crate::picker::GitChangeCandidate> {
    let mut out = Vec::new();
    for file in &generated.index.files {
        // One row per *change block*, not per hunk: a hunk routinely holds several separate edits,
        // and each is its own thing to find and jump to. A delta with no hunks has a block over its
        // placeholder line, so a binary swap or a bare `chmod` still gets a row.
        for (change_index, block) in file.changes.iter().enumerate() {
            let (start, end) = (block.start_line, block.end_line);
            let (added, removed) = (block.added, block.removed);
            // New side first, then removed, matching the file-diff rows.
            let side_of = |i: u32| {
                generated
                    .decorations
                    .patch
                    .get(i as usize)
                    .copied()
                    .flatten()
            };
            let line_text = |i: u32| -> String {
                text.line(i as usize)
                    .chunks()
                    .collect::<String>()
                    .trim()
                    .to_string()
            };
            let mut lines = Vec::new();
            let mut line_of = Vec::new();
            for i in (start..end).filter(|&i| side_of(i) == Some(PatchLine::Added)) {
                lines.push(line_text(i));
                line_of.push(i);
            }
            for i in (start..end).filter(|&i| side_of(i) == Some(PatchLine::Removed)) {
                lines.push(line_text(i));
                line_of.push(i);
            }
            // A placeholder line is neither side; it is still the row's only content.
            if lines.is_empty() {
                for i in start..end {
                    lines.push(line_text(i));
                    line_of.push(i);
                }
            }
            out.push(crate::picker::GitChangeCandidate::for_patch(
                crate::picker::PatchRowTarget {
                    view: view_id,
                    // No viewport here, so no element to resolve against: this builder runs for a
                    // cursor that has not entered one.
                    file: None,
                    durable: None,
                    line_of,
                },
                file.path().to_string(),
                change_index as u32,
                start,
                added,
                removed,
                lines,
            ));
        }
    }
    out
}

/// The hunks one open buffer contributes to the Git-changes picker: a row per conflict block plus
/// the diff outside them when the file is conflicted, otherwise the combined staged+unstaged diff.
///
/// The same choice `changed_files_in_repo` makes for files on disk, so a file lists identically
/// whether or not it happens to be open.
fn change_hunks(
    conflicted: bool,
    text: &ropey::Rope,
    staged: &[crate::git::DiffHunk],
    unstaged: &[crate::git::DiffHunk],
) -> Vec<crate::git::DiffHunk> {
    if conflicted {
        // `unstaged` is already HEAD→buffer for a conflicted file: both cached blobs hold HEAD's
        // content, and `staged` is empty.
        return crate::git::conflict_change_hunks(text, unstaged.to_vec());
    }
    crate::git::compose_both(staged, unstaged)
}

/// One open buffer's contribution to the Git-changes picker, snapshotted under the lock so the
/// disk walk that follows runs lock-free. The combined HEAD→buffer hunks are the very list the
/// gutter renders, so the picker agrees with what the editor shows; `text` (a cheap rope clone)
/// supplies each add/modify hunk's preview line.
struct OpenChange {
    path_index: u32,
    relative_path: String,
    abs_path: String,
    hunks: Vec<crate::git::DiffHunk>,
    text: ropey::Rope,
    /// No HEAD blob and no index blob in the cached baseline — a wholly untracked file. Drives the
    /// `hide_untracked` filter; a staged-new file (index blob present) is `false`.
    untracked: bool,
}

/// One changed file's hunks, gathered before flattening into candidates with a stable file order.
struct FileChanges {
    path_index: u32,
    relative_path: String,
    abs_path: String,
    hunks: Vec<HunkInfo>,
    untracked: bool,
}

/// The per-hunk facts the Git-changes picker needs, reduced from a [`crate::git::DiffHunk`].
struct HunkInfo {
    line: u32,
    stage: aether_protocol::viewport::DiffStage,
    added: u32,
    removed: u32,
    /// The hunk's changed lines (trimmed) the query greps over and the row previews: new-side
    /// (added/modified) lines first, then the removed baseline lines. `lines[0]` is the default
    /// preview (= the old first-changed-line behaviour).
    lines: Vec<String>,
}

/// Reduce a hunk to its picker facts, pulling each new-side line from `line_at`.
fn hunk_info(h: &crate::git::DiffHunk, line_at: impl Fn(u32) -> String) -> HunkInfo {
    let mut lines: Vec<String> = (h.anchor_line..h.anchor_line + h.new_lines)
        .map(line_at)
        .collect();
    lines.extend(h.deleted.iter().map(|s| s.trim().to_string()));
    HunkInfo {
        line: h.anchor_line,
        stage: h.stage,
        added: h.new_lines,
        removed: h.deleted.len() as u32,
        lines,
    }
}

/// The 0-based `line` of a rope, trimmed; empty past the end.
fn rope_line_trimmed(text: &ropey::Rope, line: u32) -> String {
    if (line as usize) >= text.len_lines() {
        return String::new();
    }
    text.line(line as usize).to_string().trim().to_string()
}

/// Build the Git-changes picker's candidate list: one entry per hunk of every changed file **under
/// the workspace's roots**, grouped by file. Open buffers drive their own files (live combined
/// hunks plus text); the rest are diffed off disk (combined staged+unstaged vs HEAD, untracked =
/// whole-file add). Runs entirely off the lock — the status walks and per-file diffs must not sit
/// on the keystroke path's mutex. Best-effort: a root outside a repo contributes nothing.
///
/// **Workspace-scoped, aggregated across repos.** The roots may span several repos, or several
/// roots may share one; either way this is "what have I changed in what I'm working on", the list
/// form of the `c`/`Alt-c` hunk motions and the sibling of the workspace diagnostics list. Rows are
/// root-addressed (`path_index` + `relative_path`) like grep hits, so multi-root workspaces get the
/// disambiguating root label for free and nothing here needs to name a repo. A change elsewhere in
/// a root's repo — the root-is-a-subdirectory case — is a *git* question rather than a workspace
/// one and is deliberately absent; the buffer still gets a baseline if you open it
/// ([`crate::state::WorkspaceEntry::git_eligible`]).
///
/// The walk is **per repo, not per root**: discovery plus a full `statuses` pass is the expensive
/// part, and two roots in one repo used to pay it twice.
fn build_git_change_candidates(
    roots: &[std::path::PathBuf],
    open: Vec<OpenChange>,
) -> Vec<picker_state::GitChangeCandidate> {
    // Every file with a live buffer is driven by that buffer — even one that's currently clean —
    // so the disk pass never double-lists it or surfaces on-disk changes the buffer has reverted.
    let open_keys: std::collections::HashSet<(u32, String)> = open
        .iter()
        .map(|o| (o.path_index, o.relative_path.clone()))
        .collect();

    let mut files: Vec<FileChanges> = Vec::new();

    for o in &open {
        let infos: Vec<HunkInfo> = o
            .hunks
            .iter()
            .map(|h| hunk_info(h, |l| rope_line_trimmed(&o.text, l)))
            .collect();
        if !infos.is_empty() {
            files.push(FileChanges {
                path_index: o.path_index,
                relative_path: o.relative_path.clone(),
                abs_path: o.abs_path.clone(),
                hunks: infos,
                untracked: o.untracked,
            });
        }
    }

    let mut walked: Vec<std::path::PathBuf> = Vec::new();
    for root in roots {
        let Some(identity) = crate::git::discover_repo(root) else {
            continue; // a root outside any repo contributes nothing
        };
        if walked.contains(&identity.workdir) {
            continue; // another root already walked this repo
        }
        walked.push(identity.workdir.clone());
        for changed in crate::git::changed_files_in_repo(&identity.workdir) {
            let abs = identity.workdir.join(&changed.rel_path);
            // Keep only what falls under a root: the repo's changes elsewhere aren't this
            // workspace's. This is also what re-addresses the row from repo- to root-relative.
            let Some((path_index, relative_path)) =
                crate::workspace_index::workspace_relative_parts(&abs, roots)
            else {
                continue;
            };
            if open_keys.contains(&(path_index, relative_path.clone())) {
                continue; // a live buffer drives this file instead
            }
            let working_lines: Vec<&[u8]> = changed.working.split(|&b| b == b'\n').collect();
            let infos: Vec<HunkInfo> = changed
                .hunks
                .iter()
                .map(|h| {
                    hunk_info(h, |l| {
                        working_lines
                            .get(l as usize)
                            .map(|b| String::from_utf8_lossy(b).trim().to_string())
                            .unwrap_or_default()
                    })
                })
                .collect();
            files.push(FileChanges {
                path_index,
                relative_path,
                abs_path: abs.to_string_lossy().into_owned(),
                hunks: infos,
                untracked: changed.untracked,
            });
        }
    }

    // Stable file order (root index, then path); hunks within a file keep their anchor order.
    files.sort_by(|a, b| {
        (a.path_index, a.relative_path.as_str()).cmp(&(b.path_index, b.relative_path.as_str()))
    });
    let mut out = Vec::new();
    for f in files {
        for (i, info) in f.hunks.into_iter().enumerate() {
            out.push(
                picker_state::GitChangeCandidate::new(
                    f.path_index,
                    f.relative_path.clone(),
                    f.abs_path.clone(),
                    i as u32,
                    info.line,
                    info.stage,
                    info.added,
                    info.removed,
                    info.lines,
                )
                .with_untracked(f.untracked),
            );
        }
    }
    out
}

/// Cursor context for cursor-anchored picker centering (Grep / GitChanges / Jumplist) —
/// resolved before `pickers` is borrowed out of the state.
struct CursorCentering {
    /// Leading edge of the selection: the position "nearest candidate" is measured from.
    leading_edge: LogicalPosition,
    /// The buffer's absolute path — the jumplist keys entries by it, since its files can sit
    /// outside every root. `None` for a scratch, which `view_id` then identifies.
    abs_path: Option<String>,
    /// The view key, when the cursor sits in a materialised view — a commit's patch, the working
    /// changes. A jumplist entry captured from one matches on this rather than on the buffer id,
    /// which does not survive the view being closed and reopened.
    view_key: Option<String>,
    /// The view the client is in — the jumplist's identity for a pathless target (a scratch, a
    /// patch's own text), which can be a captured target in its own right
    /// (`crate::jumplist::location_of`). `None` when the buffer is gone.
    view_id: Option<ViewId>,
    /// The revision this buffer *is*, for a `git/show` virtual buffer — the log picker's "where
    /// you are". `None` for an ordinary file buffer.
    revision: Option<String>,
    /// Index of the **outline** entry the cursor is in, for a composed view. Resolved here, beside
    /// the other "where you are" answers, because it needs the viewport — and resolved by the same
    /// function the breadcrumb uses, so the row the picker opens on is the one the status bar is
    /// already naming.
    outline_index: Option<usize>,
}

/// What a **re-view** produces for a picker kind, and so whether it may replace what the picker
/// already holds.
///
/// A re-view is a scroll refetch, a hide/re-attach, an Explorer step — anything that is not a fresh
/// open, which removes the slot outright and never reaches this decision.
///
/// **Keyed on the kind, and exhaustively.** It used to be decided by matching the *pair* of
/// candidate variants, old against new, with an implicit fallthrough to "replace". That is a
/// partial map wearing a total one's clothes: an unlisted pair meant "discard the snapshot" with
/// nothing to notice. It bit a patch outline, which lives under `DocumentSymbols` as `GitChanges`
/// candidates and comes back from a re-view as the *symbols* placeholder — a pair nobody had
/// written down, so the list emptied itself the moment a scroll refetched it. Here a new kind
/// cannot be forgotten; the compiler asks.
enum ReViewBuild {
    /// The builder answers a re-view with an empty placeholder, so the snapshot taken on open is
    /// the only real data there is. The majority.
    Placeholder,
    /// The builder re-snapshots on every view — directory contents change, the buffer list changes,
    /// the jumplist is rebuilt from the live captured list. Take the new set.
    Rebuild,
    /// Files: the workspace index hands back the same `Arc` until it refreshes, so pointer identity
    /// decides.
    IndexSnapshot,
    /// Keybindings: the *client* ships the rows — all of them on a fresh open, none on a re-view —
    /// so emptiness is the signal and the kind alone cannot say.
    ClientSupplied,
}

fn re_view_build(kind: PickerKind) -> ReViewBuild {
    match kind {
        PickerKind::Files => ReViewBuild::IndexSnapshot,
        PickerKind::Keybindings => ReViewBuild::ClientSupplied,
        // Snapshot-on-open kinds: an LSP round-trip, a repo walk, a grep, a working-tree diff.
        // Re-running any of them mid-scroll would shuffle rows under the cursor even when it
        // succeeded, which is the reason the placeholder exists.
        PickerKind::Grep
        | PickerKind::Diagnostics
        | PickerKind::DiagnosticsWorkspace
        | PickerKind::References
        | PickerKind::DocumentSymbols
        | PickerKind::WorkspaceSymbols
        | PickerKind::GitChanges
        | PickerKind::GitChangesFile
        | PickerKind::GitBranches
        | PickerKind::GitLog
        | PickerKind::GitLogFile
        | PickerKind::GitStash
        | PickerKind::GitBaseline => ReViewBuild::Placeholder,
        // Cheap and live: re-reading them is the point, and a stale list would be the bug.
        PickerKind::Buffers
        | PickerKind::Shells
        | PickerKind::Agents
        | PickerKind::Explorer
        | PickerKind::Workspaces
        | PickerKind::LspServers
        | PickerKind::Jumplist => ReViewBuild::Rebuild,
    }
}

pub async fn picker_view(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: PickerViewParams,
) -> Result<PickerViewResult, RpcError> {
    let client_id = ctx.client_id;

    // Build candidates outside the mutation phase. Files needs an async workspace walk;
    // Views reads ServerState directly. Grep starts empty — the candidate set is generated
    // on demand by `picker/query`'s spawned search. Explorer re-lists the requested directory
    // (or the previously-listed one on resume) every call, like Views — directories change.
    // Per-kind active-workspace gating. Workspaces is allowed before activation — it's how the
    // user gets a workspace active in the first place — and Keybindings has no workspace-scoped
    // data at all (the client ships its rows), so help works pre-activation too. Files / Views /
    // Grep / Explorer require an active workspace; their candidate builders all hit workspace-scoped
    // data and would error or return nothing without one.
    if !matches!(
        params.kind,
        PickerKind::Workspaces | PickerKind::Keybindings
    ) {
        let s = state.lock().await;
        s.active_workspace_or_err(client_id)?;
    }
    // Explorer carries its committed anchor (path the query peeks relative to) + whether the peek
    // resolved, out of the candidate-building phase so the hydration phase below can persist both.
    let mut explorer_anchor_to_set: Option<(picker_state::ExplorerAnchorInfo, bool)> = None;
    // The log walk's "stopped at the cap" verdict, carried out of the candidate build so the
    // hydration phase can store it on the slot (a scroll re-view keeps the snapshot and must keep
    // reporting it). `None` on a re-view, which leaves the stored flag alone.
    let mut log_truncated: Option<bool> = None;
    let candidates = match params.kind {
        PickerKind::Files => {
            // Walk the workspace outside the global lock — on first call it can take seconds.
            // The `Arc<WorkspaceIndex>` clone is cheap; the walk itself is memoized inside.
            let (workspace_index, roots) = {
                let s = state.lock().await;
                let p = s.active_workspace_or_err(client_id)?;
                (p.workspace_index.clone(), p.paths.clone())
            };
            // The index walk is hidden-*inclusive*, so tracked dot-entries (e.g.
            // `.circleci/workflows.yml`) are reachable by default; the `hide_hidden` chip drops
            // them again during `rerank`.
            let files = workspace_index.files().await;
            // One Git status pass per workspace root, aligned to the file snapshot by index, computed
            // off the lock (statuses walks the worktree). Empty for roots that aren't in a repo.
            let git_status = std::sync::Arc::new(build_file_git_status(&files, &roots));
            picker_state::PickerCandidates::Files { files, git_status }
        }
        PickerKind::Buffers | PickerKind::Shells | PickerKind::Agents => {
            let s = state.lock().await;
            build_view_candidates(&s, client_id, params.kind)
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
                    None if params.reset == PickerReset::All => Default::default(),
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
                    params.reset == PickerReset::All,
                    &filters,
                )
                .await?;
                explorer_anchor_to_set = Some((anchor, peek_missing));
                picker_state::PickerCandidates::Explorer(listing)
            }
        }
        PickerKind::Workspaces => {
            // Configured-workspace enumeration is a synchronous read of one directory under
            // `$XDG_CONFIG_HOME/aether/workspaces/`. No active-workspace check; works pre-activation.
            let names = {
                let s = state.lock().await;
                s.workspaces_dir()
                    .and_then(|d| crate::config::list_workspace_names_in(&d))
                    .map_err(|e| RpcError::internal(format!("listing workspaces: {e}")))?
            };
            let s = state.lock().await;
            picker_state::PickerCandidates::Workspaces(workspace_candidates(&s, &names))
        }
        PickerKind::Diagnostics => match params.buffer_id {
            // Fresh open: build from the current diagnostics of everything the **view** shows.
            // For an ordinary view that is one buffer and nothing changes; for a working-changes
            // view it is every file being reviewed, which is what "here" means once "here" is a
            // view rather than a buffer.
            Some(buffer_id) => {
                let s = state.lock().await;
                let mut out = Vec::new();
                for id in
                    crate::handlers::viewport::view_element_buffers(&s, params.view_id, buffer_id)
                {
                    out.extend(build_diagnostic_candidates(&s, id));
                }
                picker_state::PickerCandidates::Diagnostics(out)
            }
            // Resume / scroll re-view: an empty placeholder; `preserve_existing` keeps the snapshot.
            None => picker_state::PickerCandidates::Diagnostics(Vec::new()),
        },
        // Modal sibling of GitChanges: workspace-wide, rebuilt fresh on every view from the stored
        // diagnostics (open buffers' live set + the path-keyed closed-file set). All in-memory, so
        // it's synchronous — there's no slow LSP round-trip (no server answers the workspace pull).
        PickerKind::DiagnosticsWorkspace => {
            let s = state.lock().await;
            picker_state::PickerCandidates::Diagnostics(build_workspace_diagnostic_candidates(
                &s, client_id,
            ))
        }
        PickerKind::LspServers => {
            // Rebuilt every view from the active workspace's servers — the set is tiny and statuses
            // change, so there's no snapshot to preserve.
            let s = state.lock().await;
            let workspace = s.active_workspace_or_err(client_id)?;
            let (workspace_id, roots) = (workspace.id.clone(), workspace.paths.clone());
            picker_state::PickerCandidates::LspServers(build_lsp_server_candidates(
                &s,
                &workspace_id,
                &roots,
            ))
        }
        // References always starts empty: the `textDocument/references` resolve is slow (an LSP
        // round-trip), so the picker opens immediately and a spawned task (below, after the lock
        // is set up) fills it. On a fresh open the empty set is installed here; on a resume/scroll
        // re-view `preserve_existing` keeps the prior snapshot.
        PickerKind::References => picker_state::PickerCandidates::References(Vec::new()),
        // DocumentSymbols also resolves asynchronously (a `textDocument/documentSymbol` round-trip),
        // so it opens empty and the spawned task (below) fills it; resume/scroll re-views preserve
        // the prior snapshot via `preserve_existing`.
        // A composed view's outline is its **files**; an ordinary buffer's is its document symbols,
        // which arrive asynchronously (hence the empty open). Asked of the *view*, because the
        // outline describes what is on screen — the focused element's symbols describe one hunk's
        // file and nothing else about the review.
        PickerKind::DocumentSymbols => {
            // Needs the *viewport*, not just the view: an outline entry's line is a line of the
            // element's buffer, and only the viewport knows which buffer each element windows.
            let outline = match params.view_id {
                Some(view_id) => {
                    let s = state.lock().await;
                    let vp = s
                        .viewports
                        .values()
                        .find(|v| v.client_id == client_id && v.view_id == view_id);
                    vp.and_then(|v| {
                        let doc = s.try_doc_of(s.view_of(v).presenting)?;
                        // Any *generated* view has an outline of its own — a patch's hunks, a
                        // shell's runs — and `view_outline` is the one place that says which.
                        // Asking "is it a patch?" here is what made `Space o` in a shell fall
                        // through to a language server that has nothing to say about it.
                        doc.generated.as_ref()?;
                        Some(build_outline_candidates(&s, v, &doc.text))
                    })
                }
                None => None,
            };
            match outline {
                Some(rows) => picker_state::PickerCandidates::GitChanges(rows),
                None => picker_state::PickerCandidates::Symbols(Vec::new()),
            }
        }
        // Workspace symbols are query-driven: the picker opens empty and each `picker/query` fans
        // out afresh, exactly as Grep does.
        PickerKind::WorkspaceSymbols => {
            picker_state::PickerCandidates::WorkspaceSymbols(Vec::new())
        }
        // Repo-scoped, not root-scoped: the list is one repo's working-tree changes, including
        // files outside every workspace root when a root is a subdirectory of its repo. Resolved as
        // a *writable* repo — the same rule `git/prepare_commit` and the branch picker use, so a
        // single-repo workspace never sees a chooser, and the list can't name changes you could
        // never stage or commit.
        PickerKind::GitChanges if params.reset == PickerReset::All => {
            // Snapshot the roots + every in-root open buffer (live combined hunks + text) under a
            // brief lock, then build off-lock — the repo walks + per-file diffs must not block the
            // keystroke path. A snapshot of the working trees at open.
            let (roots, open) = {
                let s = state.lock().await;
                let workspace = s.active_workspace_or_err(client_id)?;
                let (workspace_id, roots) = (workspace.id.clone(), workspace.paths.clone());
                let open: Vec<OpenChange> = s
                    .buffers_in_workspace(&workspace_id)
                    .into_iter()
                    .filter_map(|id| {
                        let b = s.documents.get(&s.buffers.get(&id)?.document)?;
                        let abs = b.canonical_path.as_deref()?;
                        // In-root buffers only: a guest buffer's changes aren't the workspace's,
                        // and a scratch buffer has no path at all.
                        let (path_index, relative_path) =
                            crate::workspace_index::workspace_relative_parts(abs, &roots)?;
                        // Combined staged+unstaged hunks from the cached baseline + the LIVE buffer
                        // text — recomputed here rather than read from `git_both_hunks`, which only
                        // refreshes while the inline diff view is on, so unsaved edits always show.
                        // Same shape as the disk path: a `None` index blob (untracked) diffs the
                        // whole buffer as an addition.
                        // A *pending* baseline contributes nothing rather than a whole-file
                        // addition: `None` drops the buffer from `open`, so the disk pass covers
                        // the file instead — accurate, since a buffer whose baseline is still
                        // loading has had no time to be edited.
                        let content = s.git_baseline.get(&id).and_then(|b| b.content())?;
                        let (head, index, staged, conflicted) = (
                            content.blob.is_some(),
                            content.index_blob.clone(),
                            content.staged_hunks.clone(),
                            content.conflicted,
                        );
                        let unstaged = crate::git::hunks_from_buffers(
                            index.as_deref().unwrap_or(b""),
                            b.text.to_string().as_bytes(),
                        );
                        Some(OpenChange {
                            path_index,
                            relative_path,
                            abs_path: abs.to_string_lossy().into_owned(),
                            hunks: change_hunks(conflicted, &b.text, &staged, &unstaged),
                            text: b.text.clone(),
                            untracked: !head && index.is_none(),
                        })
                    })
                    .collect();
                (roots, open)
            };
            picker_state::PickerCandidates::GitChanges(build_git_change_candidates(&roots, open))
        }
        // Scroll / resume re-view: an empty placeholder that `preserve_existing` keeps, like
        // Diagnostics. The list is a snapshot of the working trees taken at open, so re-walking
        // them on every page would be both wasteful and (worse) able to change the rows under a
        // scroll.
        PickerKind::GitChanges => picker_state::PickerCandidates::GitChanges(Vec::new()),
        PickerKind::GitChangesFile => {
            // Modal sibling of GitChanges, locked to the active buffer (`params.buffer_id`): just
            // that buffer's live hunks, no disk walk. Like Diagnostics, a fresh open carries the
            // buffer id and rebuilds; a scroll/resume re-view passes `None` and `preserve_existing`
            // keeps the snapshot.
            // A patch buffer answers the same question with its own hunks — it *is* a set of
            // changes, just one spanning several files rather than one file's working tree.
            //
            // Asked of the **view**, not the focused element. This arm existed already and was
            // almost unreachable: the client sent the focused element's buffer, which in a
            // working-changes view is one of the *files*, so `Space c` there listed that file's
            // hunks and called it "here". The view's own document is the patch, and its changes are
            // the whole review — which is the view-wide answer, from rows that already existed.
            let patch_rows = match params.buffer_id {
                Some(buffer_id) => {
                    let s = state.lock().await;
                    s.try_doc_of(buffer_id).and_then(|d| {
                        let generated = d.patch()?;
                        let view = s.view_presenting(buffer_id)?;
                        Some(build_patch_change_candidates(view, generated, &d.text))
                    })
                }
                None => None,
            };
            let open = match params.buffer_id.filter(|_| patch_rows.is_none()) {
                Some(buffer_id) => {
                    let s = state.lock().await;
                    let roots = s.active_workspace_or_err(client_id)?.paths.clone();
                    s.try_doc_of(buffer_id)
                        .and_then(|b| {
                            let abs = b.canonical_path.as_deref()?;
                            let (path_index, relative_path) =
                                crate::workspace_index::workspace_relative_parts(abs, &roots)?;
                            // Pending contributes nothing, as above: the disk pass covers it.
                            let content =
                                s.git_baseline.get(&buffer_id).and_then(|b| b.content())?;
                            let (head, index, staged, conflicted) = (
                                content.blob.is_some(),
                                content.index_blob.clone(),
                                content.staged_hunks.clone(),
                                content.conflicted,
                            );
                            let unstaged = crate::git::hunks_from_buffers(
                                index.as_deref().unwrap_or(b""),
                                b.text.to_string().as_bytes(),
                            );
                            Some(OpenChange {
                                path_index,
                                relative_path,
                                abs_path: abs.to_string_lossy().into_owned(),
                                hunks: change_hunks(conflicted, &b.text, &staged, &unstaged),
                                text: b.text.clone(),
                                untracked: !head && index.is_none(),
                            })
                        })
                        .into_iter()
                        .collect()
                }
                None => Vec::new(),
            };
            // Empty `roots` ⇒ no disk walk; only the single open buffer's hunks are built.
            match patch_rows {
                Some(rows) => picker_state::PickerCandidates::GitChanges(rows),
                None => picker_state::PickerCandidates::GitChanges(build_git_change_candidates(
                    &[],
                    open,
                )),
            }
        }
        // The client ships the rows (its keymap tables aren't server knowledge). A fresh open
        // carries them; a scroll/resume re-view sends none and `preserve_existing` keeps the
        // previously-shipped set.
        PickerKind::Keybindings => picker_state::PickerCandidates::Keybindings(
            params
                .keybindings
                .clone()
                .unwrap_or_default()
                .into_iter()
                .map(Into::into)
                .collect(),
        ),
        // Rebuilt from the live captured list on every view — a cheap in-memory clone, and the
        // backing list persists regardless of the picker, so there's nothing to resume. Opens
        // empty when nothing has been captured yet.
        PickerKind::Jumplist => {
            let s = state.lock().await;
            picker_state::PickerCandidates::Jumplist(
                s.jumplist(client_id)
                    .map(|list| list.entries.clone())
                    .unwrap_or_default(),
            )
        }
        // Resolve the repo under a brief lock, then walk its refs off it: `list_branches` peels a
        // commit per branch and opens every sibling worktree, which is filesystem work that has no
        // business on the keystroke path's mutex (same reasoning as GitChanges above).
        //
        // Resolved as a *writable* repo — the same rule `git/prepare_commit` uses, so a single-repo
        // workspace never sees a chooser. Deliberately stricter than "readable": every action this
        // picker offers is a mutation, so listing branches for a repo you could never check out in
        // would just be a dead end.
        PickerKind::GitBranches if params.reset == PickerReset::All => {
            let repo = {
                let s = state.lock().await;
                resolve_writable_repo(&s, client_id, None, params.buffer_id)?
            };
            let repo_id = repo.repo_id.clone();
            let workdir = std::path::PathBuf::from(&repo.repo_id);
            let rows =
                tokio::task::spawn_blocking(move || crate::git::branch_picker_rows(&workdir))
                    .await
                    .unwrap_or_default();
            picker_state::PickerCandidates::GitBranches(
                rows.into_iter()
                    .map(|row| picker_state::GitBranchCandidate {
                        repo_id: repo_id.clone(),
                        row,
                    })
                    .collect(),
            )
        }
        // Scroll / resume re-view: an empty placeholder that `preserve_existing` keeps, like
        // Diagnostics. Re-resolving here would be actively wrong, not just wasteful — a re-view
        // carries no `buffer_id`, and the repo comes from the buffer, so it would refuse outright
        // and fail the scroll on a picker that had already opened perfectly well.
        PickerKind::GitBranches => picker_state::PickerCandidates::GitBranches(Vec::new()),
        // The log: one repo's history, newest first. Resolved against *reachable* repos rather
        // than writable ones — reading history is not a mutation, and a repo that's only
        // reachable through an open buffer stays fully read-eligible.
        //
        // `GitLogFile` narrows to the active buffer's repo-relative path, the same
        // buffer-locked shape `GitChangesFile` has.
        kind @ (PickerKind::GitLog | PickerKind::GitLogFile)
            if params.reset == PickerReset::All =>
        {
            let (workdir, path) = {
                let s = state.lock().await;
                let repo = resolve_readable_repo(&s, client_id, params.buffer_id)?;
                let workdir = std::path::PathBuf::from(&repo.repo_id);
                // The path is repo-relative, so it survives a root that is a subdirectory of the
                // repo — the file log follows the file, not the workspace's view of it.
                let path = match kind {
                    PickerKind::GitLogFile => {
                        let doc = params.buffer_id.and_then(|b| s.try_doc_of(b));
                        let from_disk =
                            doc.and_then(|d| d.canonical_path.clone()).and_then(|abs| {
                                abs.strip_prefix(&workdir).ok().map(|rel| {
                                    rel.components()
                                        .map(|c| c.as_os_str().to_string_lossy())
                                        .collect::<Vec<_>>()
                                        .join("/")
                                })
                            });
                        // A `git/show <rev>:<path>` buffer has no file on disk but does name a
                        // path — asking for "this file's history" from one is a reasonable thing
                        // to do, so take the path out of its key rather than refusing.
                        let from_revision = doc
                            .and_then(|d| d.virtual_source.as_ref())
                            .and_then(|v| v.target.path())
                            .map(str::to_string);
                        Some(
                            from_disk
                                .or(from_revision)
                                .ok_or_else(RpcError::buffer_has_no_path)?,
                        )
                    }
                    _ => None,
                };
                (workdir, path)
            };
            let repo_id = workdir.to_string_lossy().into_owned();
            let row_path = path.clone();
            let (rows, truncated) = tokio::task::spawn_blocking(move || {
                crate::git::log_commits(&workdir, path.as_deref(), LOG_MAX_EXAMINED)
            })
            .await
            .unwrap_or_default();
            log_truncated = Some(truncated);
            picker_state::PickerCandidates::GitLog(
                rows.into_iter()
                    .map(|row| {
                        picker_state::GitCommitCandidate::new(
                            repo_id.clone(),
                            row_path.clone(),
                            row,
                        )
                    })
                    .collect(),
            )
        }
        // Scroll / resume re-view: keep the snapshot (and its truncation verdict) rather than
        // re-walking history per page.
        PickerKind::GitLog | PickerKind::GitLogFile => {
            picker_state::PickerCandidates::GitLog(Vec::new())
        }
        // The stash list is small and changes under every stash mutation, so it rebuilds on a
        // fresh open like the branch picker rather than being preserved. Read-only, so it resolves
        // against reachable repos like the log.
        PickerKind::GitStash if params.reset == PickerReset::All => {
            let workdir = {
                let s = state.lock().await;
                std::path::PathBuf::from(
                    resolve_readable_repo(&s, client_id, params.buffer_id)?.repo_id,
                )
            };
            let repo_id = workdir.to_string_lossy().into_owned();
            let rows = tokio::task::spawn_blocking(move || crate::git::list_stashes(&workdir))
                .await
                .unwrap_or_default();
            picker_state::PickerCandidates::GitStash(
                rows.into_iter()
                    .map(|row| picker_state::GitStashCandidate {
                        repo_id: repo_id.clone(),
                        row,
                    })
                    .collect(),
            )
        }
        // Scroll / resume re-view: the empty placeholder `preserve_existing` keeps, like the
        // branch picker — re-resolving carries no `buffer_id` and would fail a scroll.
        PickerKind::GitStash => picker_state::PickerCandidates::GitStash(Vec::new()),
        // Rebuilt on a fresh open like the branch picker: both the branch rows and the `current`
        // marker go stale as soon as HEAD or the baseline moves. Read-only — choosing a baseline
        // writes nothing to the repo — so it resolves against reachable repos like the log.
        PickerKind::GitBaseline if params.reset == PickerReset::All => {
            let (workdir, current) = {
                let s = state.lock().await;
                let workdir = std::path::PathBuf::from(
                    resolve_readable_repo(&s, client_id, params.buffer_id)?.repo_id,
                );
                let current = s.git_baseline_choices.get(&workdir).cloned();
                (workdir, current)
            };
            let repo_id = workdir.to_string_lossy().into_owned();
            let rows =
                tokio::task::spawn_blocking(move || crate::git::baseline_picker_rows(&workdir))
                    .await
                    .unwrap_or_default();
            picker_state::PickerCandidates::GitBaseline(
                rows.into_iter()
                    .map(|row| picker_state::GitBaselineCandidate {
                        repo_id: repo_id.clone(),
                        current: baseline_row_is_current(&row, current.as_ref()),
                        row,
                    })
                    .collect(),
            )
        }
        PickerKind::GitBaseline => picker_state::PickerCandidates::GitBaseline(Vec::new()),
    };

    let mut s = state.lock().await;
    let key = (client_id, params.kind);

    // Pre-resolve cursor info if we'll use it for cursor-anchored centering (Grep / GitChanges /
    // Jumplist). Done before borrowing `pickers` out of `s` so we don't juggle conflicting
    // borrows after the split.
    let cursor_centering_info: Option<CursorCentering> =
        match (params.kind, params.center_on_cursor) {
            (kind, Some(buffer_id)) if kind.centers_on_cursor() => {
                // Where the client is, as a place the rows can be compared with — in a composed
                // view, the file its focused element shows and the cursor's line in that file,
                // whatever the element windows.
                let here = step_location(&s, client_id, buffer_id);
                let leading_edge = here
                    .as_ref()
                    .map_or(LogicalPosition::default(), |h| h.ordered().0);
                let current_abs = here.as_ref().and_then(|h| h.path.clone());
                // A virtual buffer's key is `<repo>@<rev>[:<path>]`; the rev is what the log
                // picker centres on.
                let revision = s
                    .try_doc_of(buffer_id)
                    .and_then(|d| d.virtual_source.as_ref())
                    .and_then(|v| v.target.rev())
                    .map(str::to_string);
                let view_key = here.as_ref().and_then(|h| h.view_key.clone());
                Some(CursorCentering {
                    leading_edge,
                    abs_path: current_abs,
                    view_key,
                    view_id: here.as_ref().map(|h| h.view),
                    revision,
                    // Only meaningful for the outline, and only over a composed view; `None`
                    // everywhere else, which is what the centring arm below keys off.
                    outline_index: (kind == PickerKind::DocumentSymbols)
                        .then(|| {
                            crate::handlers::viewport::outline_entry_at(&s, client_id, buffer_id)
                                .map(|(i, _)| i)
                        })
                        .flatten(),
                })
            }
            _ => None,
        };

    // `Space Alt-/`: grep for the buffer's selection. Slice the selection text now (the same
    // derivation `search_set`'s `from_selection` does for `Alt-/`), before the `pickers`/`matcher`
    // split-borrow takes `s`. An empty selection (empty buffer) leaves grep unseeded.
    let grep_selection_query: Option<String> =
        match (params.from_selection, params.kind, params.buffer_id) {
            (true, PickerKind::Grep, Some(buffer_id)) => s.try_doc_of(buffer_id).and_then(|buf| {
                let cursor = s
                    .cursors
                    .get(&(client_id, buffer_id))
                    .copied()
                    .unwrap_or_default();
                let (start, end) = scope_range(buf, &cursor, CopyScope::Selection);
                let text = buf.text.slice(start..end).to_string();
                (!text.is_empty()).then_some(text)
            }),
            _ => None,
        };
    // Filled by the from-selection hydration below, then drained into a search spawn after the lock.
    let mut grep_search_to_spawn: Option<(String, aether_protocol::picker::PickerFilters, u64)> =
        None;

    // (Re-)hydrate picker state per the requested reset scope: `All` on a fresh open, `Keep` on a
    // re-view within one (scroll refetch, hide/re-attach, Explorer navigation), which preserves
    // whatever the prior `view`/`query`/`hide` cycle left behind. Split-borrow `pickers` and
    // `matcher` from `s` so we can hold mutable references to both at once.
    let ServerState {
        pickers, matcher, ..
    } = &mut *s;
    // A wiping open drops the slot outright — query, hits, chips, highlight. The *generation* is
    // the one thing that must not restart: grep's streaming worker is cancelled by comparing the
    // generation it was spawned with against the live picker's (see `grep::spawn_search`), and a
    // walk from the previous open keeps running after `hide`. Restarting the counter at 0 would let
    // that stale worker's generation collide with the reopened picker's and append hits for the
    // *old* query into the new list, so carry the retired slot's generation forward past it.
    let carried_generation = match params.reset {
        PickerReset::Keep => None,
        PickerReset::All => pickers.remove(&key).map(|p| p.generation + 1),
    };
    match pickers.entry(key) {
        std::collections::hash_map::Entry::Vacant(e) => {
            let mut ps = picker_state::PickerState::new(candidates);
            // GitChangesFile shares GitChanges' candidate type, so pin the slot's kind to the
            // requested one — the `picker/update` push echoes it and the client drops mismatches.
            ps.kind = params.kind;
            if let Some(generation) = carried_generation {
                ps.generation = generation;
            }
            e.insert(ps);
        }
        std::collections::hash_map::Entry::Occupied(mut o) => {
            let p = o.get_mut();
            // Files: the workspace index returns the same `Arc` until a refresh — skip the
            // rerank in that case. Views: the candidate set is fresh each call, always re-bind.
            // Grep: the persisted candidates *are* the prior search results — keep them on resume
            // (the caller passed an empty placeholder). Discard them only on `reset`, which was
            // handled by the `pickers.remove(&key)` call above. Explorer: fresh listing every call
            // (directory contents may have changed), so always re-bind and rerank.
            let preserve_existing = match re_view_build(params.kind) {
                // The workspace index hands back the same `Arc` until it refreshes, so pointer
                // identity *is* "nothing new to show".
                ReViewBuild::IndexSnapshot => matches!(
                    (&p.candidates, &candidates),
                    (
                        picker_state::PickerCandidates::Files { files: a, .. },
                        picker_state::PickerCandidates::Files { files: b, .. },
                    ) if Arc::ptr_eq(a, b)
                ),
                ReViewBuild::Placeholder => true,
                ReViewBuild::Rebuild => false,
                // The rows came from the client: it ships them on a fresh open and none on a
                // re-view, so emptiness is the signal rather than the kind.
                ReViewBuild::ClientSupplied => candidates.is_empty(),
            };
            if !preserve_existing {
                p.candidates = candidates;
                p.rerank(matcher);
            }
        }
    }
    let picker = pickers.get_mut(&key).expect("populated above");

    // A fresh log walk reports whether it stopped at the cap; a re-view leaves the stored verdict
    // alone, since it kept that same snapshot.
    if let Some(truncated) = log_truncated {
        picker.truncated = truncated;
    }

    // Commit the resolved Explorer anchor (navigation moved the directory) + the peek-missing flag.
    // Only set for actual directory listings — Roots mode leaves the prior anchor untouched so
    // re-entering a root resumes where it was.
    if let Some((anchor, peek_missing)) = explorer_anchor_to_set {
        picker.explorer_anchor = Some(anchor);
        picker.explorer_peek_missing = peek_missing;
    }

    // Replace persisted filters when the caller sent a set (`None` keeps what hide left). A change
    // re-ranks; for Grep it also drops the cached hits — they were produced under the old filters
    // and the client's follow-up `picker/query` will respawn the search.
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

    // from-selection grep (`Space Alt-/`): install the sliced selection as a literal query and kick
    // off the search in this same call — the grep analogue of `Alt-/`, but spawning the async walk
    // like References/DocumentSymbols do above. Bump the generation so the worker's pushes are
    // tagged freshly; the client adopts `result.generation`/`result.query` and keeps them. Queries
    // below the grep minimum are seeded (and shown) but not searched, matching `picker/query`.
    if let Some(query) = grep_selection_query {
        picker.query = query;
        picker.generation += 1;
        picker.candidates = picker_state::PickerCandidates::Grep(Vec::new());
        picker.ranked.clear();
        picker.last_completed_search = None;
        if picker.query.len() >= grep::MIN_QUERY_LEN {
            grep_search_to_spawn = Some((
                picker.query.clone(),
                picker.filters.clone(),
                picker.generation,
            ));
        }
    }

    // References / DocumentSymbols: a fresh open (`buffer_id` present, vs `None` on scroll/resume
    // re-views) kicks off the async LSP resolve. Mint an epoch, mark the picker loading, and
    // remember what to spawn once the lock is released — the picker is pushed empty + `ticking`
    // now, and the spawned task fills it.
    let async_resolve: Option<(PickerKind, BufferId, u64)> = match (params.kind, params.buffer_id) {
        // A patch outline is already built — it came from the view's own index, not a language
        // server — so there is nothing to resolve. Kicking off the LSP load anyway would mark the
        // picker loading and then overwrite the file rows with the focused file's symbols, which is
        // the very thing the outline is not.
        (PickerKind::DocumentSymbols, _)
            if matches!(
                picker.candidates,
                picker_state::PickerCandidates::GitChanges(_)
            ) =>
        {
            None
        }
        (PickerKind::References | PickerKind::DocumentSymbols, Some(buffer_id)) => {
            let epoch = next_async_load_epoch();
            picker.pending_async_load = Some(epoch);
            Some((params.kind, buffer_id, epoch))
        }
        _ => None,
    };

    // Cursor-derived centering: resolve the candidate nearest the buffer's cursor and use it as
    // the effective center_on (overriding any client-passed item). Lets `Space /` / `Space c` land
    // on the user's spot in the result list even when the cursor isn't sitting on a match exactly.
    // The resolution is echoed back via `effective_center_on` so the client knows what to highlight.
    let cursor_resolved_item: Option<PickerItem> =
        match (cursor_centering_info.as_ref(), &picker.candidates) {
            // The outline: open on the entry the cursor is in — the same entry the breadcrumb is
            // already naming, resolved by the same function, so the picker and the status bar
            // cannot disagree about where you are.
            (
                Some(CursorCentering {
                    outline_index: Some(i),
                    ..
                }),
                picker_state::PickerCandidates::GitChanges(c),
            ) if *i < c.len() => Some(picker.candidates.make_item(*i, Vec::new())),
            // The log: land on the commit the active buffer *is*, so opening the log from a
            // `git/show` buffer shows you where that commit sits in history. Nothing to resolve
            // from an ordinary buffer — the list then opens at the top, which is the newest commit.
            (
                Some(CursorCentering {
                    revision: Some(rev),
                    ..
                }),
                picker_state::PickerCandidates::GitLog(v),
            ) => v
                .iter()
                .position(|c| c.hash == *rev)
                .map(|idx| picker.candidates.make_item(idx, Vec::new())),
            // The same for a stash being read: a stash entry *is* a commit, so the buffer's
            // revision identifies its row exactly as it does in the log.
            (
                Some(CursorCentering {
                    revision: Some(rev),
                    ..
                }),
                picker_state::PickerCandidates::GitStash(v),
            ) => v
                .iter()
                .position(|c| c.row.oid == *rev)
                .map(|idx| picker.candidates.make_item(idx, Vec::new())),
            // A patch's rows are lines of this very buffer, so the cursor resolves against them
            // directly — no path to match on (there isn't one), and no per-file scoping either,
            // because the patch *is* the scope.
            (
                Some(CursorCentering {
                    leading_edge,
                    view_id: Some(view_id),
                    ..
                }),
                picker_state::PickerCandidates::GitChanges(c),
            ) if c
                .first()
                .is_some_and(|x| x.patch.as_ref().is_some_and(|p| p.view == *view_id)) =>
            {
                find_nearest_patch_change(c, leading_edge.line)
                    .map(|idx| picker.candidates.make_item(idx, Vec::new()))
            }
            // GitChanges: land on the hunk in the buffer's own file nearest the cursor line. No
            // fall-through to "some other file" — if the active file has no changes, leave the
            // highlight at the top rather than jumping to an unrelated file.
            (
                Some(CursorCentering {
                    leading_edge,
                    abs_path: Some(current_path),
                    ..
                }),
                picker_state::PickerCandidates::GitChanges(c),
            ) if !c.is_empty() => {
                // Matched on the absolute path: the rows are repo-addressed now, and the buffer may
                // sit outside every root while still being in the repo the picker lists.
                find_nearest_git_change(c, current_path, leading_edge.line)
                    .map(|idx| picker.candidates.make_item(idx, Vec::new()))
            }
            // Jumplist: land on the entry at-or-after the cursor, wrapping — the same
            // "where you are in the cycle" the `]`/`[` stepping derives, inclusive so the
            // just-jumped-to entry counts as current (`crate::jumplist::nearest_index`).
            (
                Some(CursorCentering {
                    leading_edge,
                    abs_path,
                    view_id: Some(view_id),
                    view_key,
                    ..
                }),
                picker_state::PickerCandidates::Jumplist(entries),
            ) if !entries.is_empty() => {
                let location = crate::jumplist::location_of(
                    abs_path.as_deref(),
                    *view_id,
                    view_key.as_deref(),
                );
                let idx = crate::jumplist::nearest_index(entries, location, *leading_edge);
                Some(picker.candidates.make_item(idx, Vec::new()))
            }
            _ => None,
        };

    // Resolve the window. `center_on` wins over `offset` and picks a frame containing the item;
    // we centre it (roughly) so a small navigation away keeps it on screen. Falls through to
    // `offset` if the item isn't currently ranked. The cursor-resolved item, when present,
    // takes precedence over the client-passed `center_on`.
    let limit = params.limit.max(1);
    let mut effective_offset = params.offset;
    let effective_center_on = cursor_resolved_item
        .or_else(|| params.center_on.clone())
        // Nothing to resolve from the cursor here, and the client can't name the row before it has
        // seen it — so the server supplies it, and everything below (framing, the echo back, the
        // client adopting it as its highlight) is the machinery that already exists.
        .or_else(|| current_state_item(picker, params.reset));
    if let Some(item) = effective_center_on.as_ref() {
        // Collapsible kinds: framing an item implies revealing it — expand its group before
        // resolving the row, so a centred open (`Space c` landing on the cursor's hunk) frames a
        // visible row rather than a collapsed header. Centering on a `Group` row just frames the
        // header; it expands nothing.
        if picker.collapsible() && !matches!(item, PickerItem::Group { .. }) {
            if let Some(group_key) = picker.group_key_of_item(item) {
                picker.expansion.set(group_key.clone(), true);
                picker.focus = Some(group_key);
            }
        }
        // `row_of` == the ranked position for the flat / derived-header kinds; row space for
        // the collapsible ones.
        if let Some(row) = picker.row_of(item) {
            let half = limit / 2;
            effective_offset = row.saturating_sub(half);
        }
    }
    let total = picker.total_rows();
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
    // client shows the loading state instead of an empty result set. A from-selection grep is the
    // same shape (search spawning below), so mark it ticking too and send the window count-only
    // (`items: None`) so the client renders "Searching…" rather than "No results" in the gap.
    if async_resolve.is_some() || grep_search_to_spawn.is_some() {
        if let Some(ref mut u) = update {
            u.ticking = true;
            if grep_search_to_spawn.is_some() {
                u.items = None;
                u.groups.clear(); // spans describe `items`; meaningless without them
            }
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
    // Jumplist: tell the client whether this capture is worth path-scoping (gates its dir/glob
    // chips). Derived from the entries on every view — the list only changes via re-capture,
    // and each view rebuilds candidates from it, so there's no stored flag to go stale.
    let path_filterable = match &picker.candidates {
        picker_state::PickerCandidates::Jumplist(v) => crate::jumplist::path_filterable(v),
        _ => false,
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
        path_filterable,
        // The log walk stopped at its cap: the client says so, because the query filters what was
        // loaded and "no matches" would otherwise be indistinguishable from "older than the cap".
        // Stored on the picker so a scroll re-view (which doesn't re-walk) still reports it.
        truncated: picker.truncated,
        collapsible: picker.collapsible(),
        // Carry the window on the response too — see `PickerViewResult::update`. The push below
        // stays for redundancy (and for the async grep walk's later updates).
        update: update.clone(),
    };
    let outbound = s.clients.get(&client_id).map(|c| c.outbound.clone());
    // Grab the workspace snapshot for a from-selection grep before releasing the lock (same data
    // `picker/query` hands to `grep::spawn_search`). `None` if the active workspace somehow vanished.
    let grep_workspace = if grep_search_to_spawn.is_some() {
        s.active_workspace(client_id)
            .map(|p| (p.workspace_index.clone(), p.paths.clone()))
    } else {
        None
    };
    drop(s);

    if let (Some(sender), Some(params)) = (outbound, update) {
        let _ = sender.send(picker_update_notif(params)).await;
    }

    // from-selection grep: spawn the walk now that the empty + ticking state is on the wire,
    // mirroring `picker/query`'s spawn (the search is normally driven from there).
    if let (Some((query, filters, generation)), Some((workspace_index, roots))) =
        (grep_search_to_spawn, grep_workspace)
    {
        let files = workspace_index.files().await;
        grep::spawn_search(
            state.clone(),
            files,
            roots,
            client_id,
            query,
            filters,
            generation,
        );
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
    // Explorer re-lists the query-derived peek directory before reranking; grab the workspace roots
    // up front (an immutable borrow of `s`, before the split below hands out `pickers`/`matcher`).
    let explorer_workspace_paths = if matches!(params.kind, PickerKind::Explorer) {
        s.active_workspace(client_id).map(|p| p.paths.clone())
    } else {
        None
    };
    // WorkspaceSymbols: resolve the fan-out targets up front (immutable borrows of `s`, like the
    // Explorer roots above) — the initial push below needs them: whether anything will actually be
    // asked decides `ticking`, and the fan-out size seeds the completion counter. A fan-out of zero
    // servers settles immediately rather than spinning forever. The scoping rule lives in
    // `symbol_servers`; a `Dir` filter prunes the fan-out rather than merely filtering results — a
    // server whose root is disjoint from the scope can't contribute. The query minimum is grep's:
    // below it a per-keystroke LSP fan-out (uncancellable) is pure churn.
    let mut symbol_fanout = (matches!(params.kind, PickerKind::WorkspaceSymbols)
        && params.query.len() >= grep::MIN_QUERY_LEN)
        .then(|| {
            let workspace = s.active_workspace(client_id)?;
            let roots = workspace.paths.clone();
            let scopes = crate::symbols::scoped_dirs(&params.filters, &roots);
            let servers: Vec<_> = s
                .lsp
                .symbol_servers(&workspace.id)
                .into_iter()
                .filter(|srv| crate::symbols::dir_scope_admits(&srv.root, &scopes))
                .collect();
            Some((servers, roots))
        })
        .flatten();
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
    picker.query = params.query;
    picker.filters = params.filters;
    picker.generation = params.generation;
    // A query change resets the client's selection to row 0 — drop the focus key so group
    // stepping re-coheres with it, starting from the first run of the *new* ranking. Expansion
    // itself is deliberately *not* cleared: it's keyed by group, so a group you opened re-opens
    // when a refined query brings it back, and an `Alt-a` expand-all survives the refinement.
    picker.focus = None;
    let grep_cache_hit = matches!(params.kind, PickerKind::Grep)
        && picker
            .last_completed_search
            .as_ref()
            .is_some_and(|(q, f)| *q == picker.query && *f == picker.filters);
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
        // Workspace symbols: the query is the search, so drop the prior answers and let the
        // fan-out (spawned below, off the lock) refill them server by server. Seeded here —
        // before any spawned request can land its merge (see `seed_symbol_fanout`).
        //
        // Except when the recorded fan-out already covers everything this one would ask
        // (`symbol_fanout_covers`): the servers never see the filters — the Dir chip only
        // prunes who gets asked — so a filter-only change (a chip edit, or its live preview
        // keystrokes) re-asking the same servers with the same query would wipe and refetch
        // the exact answers we're holding. Keep them and just rerank; the fan-out below is
        // cancelled so nothing spawns and `ticking` stays off (grep's cache-hit shape).
        PickerKind::WorkspaceSymbols => {
            let covered = symbol_fanout.as_ref().is_some_and(|(servers, _)| {
                picker.symbol_fanout_covers(
                    servers
                        .iter()
                        .map(|srv| (srv.root.as_path(), srv.language.as_str())),
                )
            });
            if covered {
                symbol_fanout = None;
                picker.rerank(matcher);
            } else {
                picker.candidates = picker_state::PickerCandidates::WorkspaceSymbols(Vec::new());
                picker.ranked.clear();
                picker.seed_symbol_fanout(
                    symbol_fanout
                        .as_ref()
                        .map_or(0, |(servers, _)| servers.len()),
                );
                picker.symbol_fanned = symbol_fanout.as_ref().map(|(servers, _)| {
                    (
                        picker.query.clone(),
                        servers
                            .iter()
                            .map(|srv| (srv.root.clone(), srv.language.clone()))
                            .collect(),
                    )
                });
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
                Some(workspace_paths),
            ) = (
                &picker.candidates,
                picker.explorer_anchor.clone(),
                explorer_workspace_paths.as_ref(),
            ) {
                let (listing, peek_missing) = build_explorer_peek(
                    std::path::Path::new(&anchor.path),
                    &picker.query,
                    workspace_paths,
                    &picker.filters,
                );
                picker.candidates = picker_state::PickerCandidates::Explorer(listing);
                picker.explorer_peek_missing = peek_missing;
            }
            picker.rerank(matcher);
        }
        _ => picker.rerank(matcher),
    }

    // A new query restarts the result list at the top: the client resets its offset + selection to
    // 0, so window from 0 to match (the prior offset is meaningless against the new ranking, and a
    // mismatched offset would make the client reject the push and keep showing stale rows).
    if let Some(window) = picker.subscribed.as_mut() {
        window.offset = 0;
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
    // Only tick when at least one server will actually be asked: with an empty fan-out (no
    // capable pinned server, or the Dir scope excluded them all) nothing would ever push again,
    // so a `ticking` push here would strand the client on its loading state.
    let will_spawn_symbol_search = symbol_fanout
        .as_ref()
        .is_some_and(|(servers, _)| !servers.is_empty());
    // Mark the initial push as ticking when we're about to spawn the search. Without this the
    // client would briefly see "0 hits, search finished" between sending the query and the
    // coordinator's first batch landing. Send it as a count-only tick (`items: None`) too: the
    // results for this query aren't ready (grep just cleared its candidates / the async resolve is
    // still outstanding), so an `items: Some([])` here would blank the previous query's window on
    // every keystroke. `None` keeps it on screen until the first real batch — or the completion
    // push (always `Some(...)` from `build_update`) — replaces it. No stale rows can get stuck.
    if will_spawn_grep_search
        || will_spawn_symbol_search
        || (matches!(
            params.kind,
            PickerKind::References | PickerKind::DocumentSymbols
        ) && async_loading)
    {
        if let Some(ref mut u) = update {
            u.ticking = true;
            u.items = None;
            u.groups.clear(); // spans describe `items`; meaningless without them
        }
    }
    let outbound = s.clients.get(&client_id).map(|c| c.outbound.clone());
    let workspace_index_for_grep = if matches!(params.kind, PickerKind::Grep) {
        // Active-workspace lookup can fail in the (defensively-handled) case where the client
        // somehow lost its active workspace between opening the picker and querying it. Skip the
        // grep spawn in that case — there's nothing meaningful to search.
        s.active_workspace(client_id)
            .map(|p| (p.workspace_index.clone(), p.paths.clone()))
    } else {
        None
    };
    // Captured before the lock goes: the fan-out below is counted as deferred work,
    // and the count has to be taken while this handler is still running — its reply is
    // sent after it returns, so anything counted here is outstanding before the client
    // can observe the response.
    let deferred = s.deferred.clone();
    drop(s);

    if let (Some(sender), Some(params)) = (outbound, update) {
        let _ = sender.send(picker_update_notif(params)).await;
    }

    if let Some((servers, roots)) = symbol_fanout {
        let token = deferred.start();
        for server in servers {
            let state = state.clone();
            let query = query_for_grep.clone();
            let roots = roots.clone();
            let token = token.clone();
            tokio::spawn(async move {
                let _token = token;
                let found = crate::symbols::query_server(&server, &query, &roots).await;
                crate::symbols::merge_results(&state, client_id, generation, found).await;
            });
        }
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
    // Two answers, in order of preference. A jumplist row captured from a composed view belongs
    // *in* that view — resolved from the **entry**, not from the ordinary select result, because a
    // view-addressed row has no path and no live buffer id and so has no ordinary result at all.
    let landing = jumplist_landing(picker, &params.item);
    let ordinary = picker_state::resolve_select(picker, &params.item);
    drop(s);
    if let Some((view_key, identity, position)) = landing {
        match land_in_captured_view(
            state,
            ctx,
            client_id,
            &view_key,
            &identity,
            position.line,
            true, // Enter on a row is a request to go there; reopening the view is how
        )
        .await?
        {
            Landing::Seated {
                open,
                seat,
                position,
            } => {
                return Ok(PickerSelectResult::ViewElement {
                    element: seat.element,
                    buffer_id: seat.buffer_id,
                    position,
                    open: open.map(Box::new),
                })
            }
            Landing::Gone { open } => {
                return Ok(PickerSelectResult::Gone {
                    open: open.map(Box::new),
                })
            }
            Landing::NoView => {}
        }
    }
    ordinary.ok_or_else(|| {
        RpcError::invalid_params(
            "selected item is not in the picker's candidate set, or is not selectable",
        )
    })
}

/// What a jumplist row needs in order to land in the view it was captured from: that view's key,
/// how the row names its element's buffer, and where in it.
fn jumplist_landing(
    picker: &crate::picker::PickerState,
    item: &PickerItem,
) -> Option<(String, String, LogicalPosition)> {
    let PickerItem::JumplistEntry { index, .. } = item else {
        return None;
    };
    let crate::picker::PickerCandidates::Jumplist(entries) = &picker.candidates else {
        return None;
    };
    let entry = entries.get(*index as usize)?;
    Some((
        entry.view.clone()?,
        entry.target.identity()?.to_string(),
        entry.position?,
    ))
}

/// Where a jumplist row captured from a composed view lands.
enum Landing {
    /// In the view: which element, and where in its buffer. `open` is the view itself, reopened,
    /// when nothing was showing it — the client adopts it before seating.
    Seated {
        open: Option<ViewOpenResult>,
        seat: aether_protocol::viewport::ViewSeat,
        position: LogicalPosition,
    },
    /// The view was reached — on screen, or reopened (`open`) — and no longer holds the entry:
    /// the change was staged, committed or reverted since the capture. Nowhere to land, and
    /// nowhere else to go: the entry is a place *in that view*.
    Gone { open: Option<ViewOpenResult> },
    /// Not on screen and not to be reopened here.
    NoView,
}

/// Where a jumplist row captured from a composed view lands — reopening that view when nothing is
/// showing it.
///
/// **The one decision.** Enter on the row and `]` onto it must land in the same place, and they used
/// to work it out separately: one named an element, the other a patch line, and each had its own
/// idea of what to do when the view was gone. Two derivations of "where does this row land" is what
/// every bug in this area has been, so there is now one, and both routes call it.
///
/// An entry that names a view is never answered with anything but that view. It used to fall back
/// to opening the entry's file in a plain editor when the view could not place it — which is where
/// every jump went the moment the view's elements stopped windowing their files — so the same
/// key took you to the review or out of it depending on state nobody could see. Now the view says
/// [`Landing::Gone`], and the client says so.
///
/// `reopen` is the caller's licence to materialise: a step that was told not to open anything still
/// wants a seat if the view happens to be on screen, but must not conjure it back otherwise.
async fn land_in_captured_view(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    client_id: ClientId,
    view_key: &str,
    identity: &str,
    line: u32,
    reopen: bool,
) -> Result<Landing, RpcError> {
    {
        let s = state.lock().await;
        if let Some((seat, position)) =
            crate::handlers::viewport::element_holding(&s, client_id, view_key, identity, line)
        {
            return Ok(Landing::Seated {
                open: None, // already on screen: nothing to open
                seat,
                position,
            });
        }
        // On screen, but without the entry.
        if s.viewports.values().any(|vp| {
            vp.client_id == client_id
                && crate::handlers::viewport::view_key_of(&s, vp.view_id).as_deref()
                    == Some(view_key)
        }) {
            return Ok(Landing::Gone { open: None });
        }
    }
    if !reopen {
        return Ok(Landing::NoView);
    }
    let opened = match crate::handlers::nav::materialise_virtual_key(state, ctx, view_key).await {
        Some(Ok(opened)) => opened,
        // A working tree gone clean has nothing left to show: the entry, and every other entry of
        // the view, is gone.
        Some(Err(e)) if e.is_nothing_to_show() => return Ok(Landing::Gone { open: None }),
        Some(Err(e)) => return Err(e),
        None => return Ok(Landing::Gone { open: None }),
    };
    let s = state.lock().await;
    Ok(seat_in_reopened(&s, opened, identity, line))
}

/// Seat in a view that has just been (re)materialised — the elements are rebuilt with it, which is
/// why an entry stores a key and a file and never an element id.
fn seat_in_reopened(
    s: &ServerState,
    mut opened: ViewOpenResult,
    identity: &str,
    line: u32,
) -> Landing {
    let Some((seat, position)) =
        crate::handlers::viewport::seat_in_fresh_view(s, opened.buffer_id, identity, line)
    else {
        return Landing::Gone { open: Some(opened) };
    };
    // **Frame the reopen on the seat.** The client subscribes to the view it has just adopted, and
    // a fresh subscribe takes its focused element from the one the scroll names — so a reopen
    // framed at the top focuses element 0 and quietly undoes the focus this jump asked for. That is
    // what "the first `]` takes me to the view but not to the entry" was: the seat was correct and
    // then overwritten a moment later by the client's own subscribe.
    opened.scroll = Some(aether_protocol::viewport::ScrollPosition {
        element: seat.element,
        line: position.line,
        sub_row: 0.0,
    });
    Landing::Seated {
        open: Some(opened),
        seat,
        position,
    }
}

/// Re-seat a jumplist jump inside the view it was captured from.
///
/// A jumplist entry addresses the element's *file* buffer, which is what makes it durable across
/// the patch being rebuilt. But the picker it was captured from resolves the same row to a
/// `ViewElement`, and a jump that behaves differently from the picker that populated it is the
/// thing the two are supposed to agree on. So while the source view is still open, upgrade the
/// jump the same way — and when it isn't, the `BufferAt` underneath is a real address that opens
/// the file at the same line.
pub async fn picker_hide(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: PickerHideParams,
) -> Result<(), RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    if let Some(picker) = s.pickers.get_mut(&(client_id, params.kind)) {
        // Closing throws the picker's state away rather than parking it. Nothing would ever read it
        // again — every `PickerReset::Keep` view is sent by a picker that's still attached, and the
        // next open wipes regardless — so this makes "closed ⇒ empty" a server-side invariant
        // instead of something each client open has to honour.
        //
        // The generation bump is the part that does real work: it's how an in-flight `grep` walk
        // learns it has been superseded (`grep::spawn_search` compares against the live picker's on
        // every batch). Without it, dismissing a search left the walk scanning the whole workspace
        // to completion, appending hits to a list nobody would see and still emitting count-only
        // pushes to a client with no picker open. The slot itself stays as the generation holder —
        // removing it would restart the counter at 0 and let that same walk collide with the next
        // open's generation.
        picker.subscribed = None;
        picker.generation += 1;
        picker.query.clear();
        picker.filters = Default::default();
        picker.candidates.clear();
        picker.ranked.clear();
        picker.last_completed_search = None;
        picker.symbol_fanned = None;
        picker.pending_async_load = None;
        picker.seed_symbol_fanout(0);
        picker.expansion.clear();
        picker.focus = None;
    }
    Ok(())
}

/// Expand, collapse or move between the groups of a collapsible picker. Groups start collapsed and
/// any number can be open at once; `Expand`/`Collapse` address one by its header (a key press on a
/// header row the client holds, a click), `Step` moves the focus to the adjacent run — resolved
/// here so it works past the fetched window — optionally opening the one it lands on (the
/// item-level spill), and `ToggleAll` opens or closes everything. Replies with the focused run's
/// place in the reshaped row space (the client picks its landing row from it) and pushes the
/// reshaped window through the normal `picker/update` path; the client's offset/generation guards +
/// refetch reconcile, so response/push arrival order doesn't matter.
pub async fn picker_set_group(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: PickerSetGroupParams,
) -> Result<PickerSetGroupResult, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    let ServerState {
        pickers, matcher, ..
    } = &mut *s;
    // A missing slot or a non-collapsible view is a benign no-op (`run: None`), matching
    // `picker/query`'s lenient style — the picker may have raced a close.
    let Some(picker) = pickers.get_mut(&(client_id, params.kind)) else {
        return Ok(PickerSetGroupResult { run: None });
    };
    if !picker.collapsible() {
        return Ok(PickerSetGroupResult { run: None });
    }
    // Resolve the target group against the current ranking FIRST: acting on one that re-ranked
    // away mid-flight — or a step off the ends — is a benign no-op (`run: None`) that must not
    // disturb the expansion state. A vanished key left in `expansion` would surprise-expand if a
    // later streaming batch brought the group back.
    let resolve = |picker: &picker_state::PickerState, header: &GroupHeader| {
        let key = picker_state::group_key_of_header(header);
        picker
            .first_candidate_of_group(&key)
            .is_some()
            .then_some(key)
    };
    let group_key = match &params.action {
        PickerGroupAction::Expand { header } | PickerGroupAction::Collapse { header } => {
            resolve(picker, header)
        }
        PickerGroupAction::Step { direction, .. } => picker.step_group_key(*direction),
        // Nothing to address: the toggle acts on every group and leaves the focus where it is.
        // A fresh picker has no focus key yet — the layout falls back to the first run, so adopt
        // that as the focus the reply will describe.
        PickerGroupAction::ToggleAll => picker.focus.clone().or_else(|| picker.first_group_key()),
    };
    let Some(group_key) = group_key else {
        return Ok(PickerSetGroupResult { run: None });
    };
    match &params.action {
        PickerGroupAction::Expand { .. } => picker.expansion.set(group_key.clone(), true),
        PickerGroupAction::Collapse { .. } => picker.expansion.set(group_key.clone(), false),
        // A spill walks into the neighbour's items, so it has to open it; a group-level step just
        // moves the highlight between headers.
        PickerGroupAction::Step { expand, .. } => {
            if *expand {
                picker.expansion.set(group_key.clone(), true);
            }
        }
        // Open everything, unless everything is already open — then close it. The server decides
        // the direction because only it sees the whole run list.
        PickerGroupAction::ToggleAll => {
            let any_collapsed = picker
                .row_layout()
                .is_some_and(|layout| layout.expanded.iter().any(|&e| !e));
            picker.expansion.set_all(any_collapsed);
        }
    }
    picker.focus = Some(group_key);
    // The focused run's geometry in the reshaped space — the fresh layout resolves the key we just
    // installed. The client picks its landing row from it (the header for group nav and collapses,
    // the run's first/last item for a descend or an item-level spill — the latter is why the
    // *length* rides the reply rather than just the header row).
    let run = picker
        .row_layout()
        .and_then(|layout| layout.focus.map(|pos| layout.run_rows(pos)));
    let update = picker_state::build_update(picker, matcher);
    let outbound = s.clients.get(&client_id).map(|c| c.outbound.clone());
    drop(s);
    if let (Some(sender), Some(update)) = (outbound, update) {
        let _ = sender.send(picker_update_notif(update)).await;
    }
    Ok(PickerSetGroupResult { run })
}

/// The `view/open` params that jump to a captured results entry: transient, cursor landing
/// exactly as selecting the source row would (`jump_to` + `jump_to_anchor`), origin recorded on
/// nav history. Entries missing workspace-relative parts (references into dependency sources)
/// get them re-derived from the active workspace, falling back to an absolute-path (external
/// buffer) open — the same routing the client's own open flow applies.
fn jumplist_open_params(
    s: &ServerState,
    client_id: ClientId,
    entry: &crate::jumplist::JumplistEntry,
    origin: BufferId,
) -> ViewOpenParams {
    // A pathless entry (a captured scratch) names its view, exactly as the picker's own select
    // does — there is no path to route through the workspace.
    let Some(abs_path) = entry.abs_path() else {
        return ViewOpenParams {
            view_id: entry.target.view_id(),
            record_nav_from: Some(origin),
            ..Default::default()
        };
    };
    let (mut path_index, mut relative_path) = match entry.target.relative_parts() {
        Some((i, rel)) => (Some(i), Some(rel.to_string())),
        None => (None, None),
    };
    if relative_path.is_none() {
        if let Some(workspace) = s.active_workspace(client_id) {
            if let Some((i, rel)) = crate::workspace_index::workspace_relative_parts(
                std::path::Path::new(abs_path),
                &workspace.paths,
            ) {
                path_index = Some(i);
                relative_path = Some(rel);
            }
        }
    }
    let absolute_path = relative_path.is_none().then(|| abs_path.to_string());
    ViewOpenParams {
        path_index,
        relative_path,
        absolute_path,
        // `None` for a whole-target entry: the open then restores the cursor (and scroll) this
        // client last had in that buffer, or the top of the file if it has never opened it —
        // which is precisely what selecting the row in the Files/view picker does.
        jump_to: entry.position,
        jump_to_anchor: entry.anchor,
        transient: Some(true),
        record_nav_from: Some(origin),
        ..Default::default()
    }
}

/// Snapshot the open picker's filtered results into this client's jumplist. Doesn't navigate — the
/// client follows up by opening the Jumplist picker framed on the returned `index`, and Enter there
/// jumps through the ordinary select path. `None` when the picker has nothing to capture — the
/// previously captured list survives. Replaces any prior capture otherwise; capturing from the
/// Jumplist picker itself narrows the list to its current subset.
pub async fn jumplist_capture(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: JumplistCaptureParams,
) -> Result<Option<JumplistCaptureResult>, RpcError> {
    let client_id = ctx.client_id;
    let mut guard = state.lock().await;
    let s = &mut *guard;
    let Some(picker) = s.pickers.get(&(client_id, params.kind)) else {
        return Err(RpcError::new(
            ErrorCode::INVALID_REQUEST,
            "no active picker for this client",
        ));
    };
    // A collapsible picker's selection can sit on a group's header row; anchor the capture on that
    // run's first item (the capture itself spans the whole filtered set either way — collapse is
    // view state, not a filter).
    let ci = match &params.item {
        PickerItem::Group { header, .. } => picker
            .first_candidate_of_group(&picker_state::group_key_of_header(header))
            .ok_or_else(|| {
                RpcError::invalid_params("the anchor group is not in the picker's result set")
            })?,
        item => picker.candidates.position_of(item).ok_or_else(|| {
            RpcError::invalid_params("selected item is not in the picker's candidate set")
        })?,
    } as u32;
    // The view these rows are rows *of*, taken while it is still open — every patch row names it,
    // and they all name the same one. Read before the capture, which ends the picker borrow.
    let patch_buffer = match &picker.candidates {
        crate::picker::PickerCandidates::GitChanges(v) => v
            .iter()
            .find_map(|c| s.try_presenting_buffer(c.patch.as_ref()?.view)),
        _ => None,
    };
    let Some((mut list, candidate_indices)) = crate::jumplist::capture(picker, &mut s.matcher)
    else {
        return Ok(None);
    };
    // **A buffer id is not a durable address for a file.** A patch row addresses the element's
    // buffer, because the patch itself was materialised and has no path to reopen — but those
    // element buffers are *transient*: opening any other editor hides the view and closes them, and
    // every entry captured from it then named a buffer that no longer existed (`unknown buffer_id`
    // on the step, which is what "the jumplist stops working once you look at something else" was).
    //
    // So give any entry whose buffer has a path the path instead. This restores the invariant the
    // rest of the jumplist already keeps — a `Buffer` target means *pathless* (the view picker
    // captures pathed rows as `File` for exactly this reason) — which is also what `location_of`
    // assumes when it decides how the current buffer identifies itself.
    // The view each row came from, named durably. Taken from the picker's own view rather than the
    // rows: every row of a patch picker is a row *of that view*.
    let view_key = patch_buffer.and_then(|b| crate::handlers::viewport::buffer_view_key(s, b));
    for entry in &mut list.entries {
        // Every row of a patch picker is a row of that view, whatever it addresses.
        if view_key.is_some() {
            entry.view = view_key.clone();
        }
        let Some(buffer_id) = entry
            .target
            .view_id()
            .and_then(|view| s.try_presenting_buffer(view))
        else {
            continue;
        };
        let doc = s.try_doc_of(buffer_id);
        let abs_path = doc
            .and_then(|d| d.canonical_path.as_ref())
            .map(|p| p.to_string_lossy().into_owned());
        // A **virtual** buffer — a commit's patch, a file at a revision — has no path either, and
        // its id is just as transient. Its durable address is the view it *is*, and `position` is
        // already a line of that view's own document. This is what a jumplist captured from a
        // commit patch's outline needs: those rows have no working-tree file behind them at all.
        let virtual_key = doc
            .and_then(|d| d.virtual_source.as_ref())
            .map(|src| src.target.key());
        entry.target = match (abs_path, virtual_key) {
            (Some(abs_path), _) => crate::jumplist::JumplistTarget::File {
                path_index: None,
                relative_path: None,
                abs_path,
            },
            (None, Some(key)) => crate::jumplist::JumplistTarget::View { key },
            // Genuinely pathless and not a view (a scratch buffer): the id is the only address.
            (None, None) => continue,
        };
    }
    // Give every entry its file identity: derive workspace-relative parts from `abs_path` (so the
    // open resolves the file rather than the root directory) and a group header for the headerless
    // buffer-scoped sources (so the picker shows which file each row belongs to). Runs even with
    // no roots to relativize against (out-of-workspace entries get absolute-path labels): a
    // grouped list's row space is collapsible, and that keys *every* row.
    //
    // Skipped entirely for an ungrouped capture (the file-shaped pickers, whose rows are whole
    // targets already carrying their parts): it would attach a per-file header above each row
    // that *is* that file, and flip the view to a collapsible list of one-row groups.
    if list.grouped {
        let roots = s
            .active_workspace(client_id)
            .map(|w| w.paths.clone())
            .unwrap_or_default();
        crate::jumplist::assign_file_groups(&mut list.entries, &roots);
    }
    // A re-capture from the Jumplist picker narrows the list but keeps describing what the
    // entries are entries *of*: the original source kind and query, not the narrowing query
    // typed into the Jumplist picker.
    if params.kind == PickerKind::Jumplist {
        if let Some(prior) = s.jumplist(client_id) {
            list.source = prior.source;
            list.query = prior.query.clone();
        }
    }
    // The highlighted row is normally in the capture; a DocumentSymbols ancestor context row
    // isn't (capture keeps only real matches) — frame the nearest captured entry at or after
    // it in candidate order instead.
    let index = candidate_indices
        .iter()
        .position(|&c| c == ci)
        .or_else(|| {
            candidate_indices
                .iter()
                .enumerate()
                .filter(|(_, &c)| c > ci)
                .min_by_key(|(_, &c)| c)
                .map(|(i, _)| i)
        })
        .unwrap_or(0);
    let total = list.entries.len() as u32;
    // Replaces whatever this context held — including a list another client captured into it. A
    // capture has always been a wholesale replacement; sharing the list makes that reach one window
    // further, and the status bar's source/query label says which capture you are stepping.
    s.set_jumplist(client_id, list);
    let pushes = jumplist_changed_pushes(s, client_id);
    drop(guard);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    Ok(Some(JumplistCaptureResult {
        total,
        index: index as u32,
    }))
}

/// Discard the context's captured list — Normal-mode `Space Alt-j`. See
/// [`aether_protocol::jumplist::JumplistClear`]. Never fails: clearing a list that isn't there (no
/// capture yet, or no active workspace at all) reports `cleared: 0` rather than an error, which is
/// what lets the client toast "already empty" instead of "clear failed".
pub async fn jumplist_clear(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: JumplistClearParams,
) -> Result<JumplistClearResult, RpcError> {
    let client_id = ctx.client_id;
    let mut guard = state.lock().await;
    let s = &mut *guard;
    let cleared = s
        .active_workspace_mut(client_id)
        .and_then(|entry| entry.jumplist.take())
        .map_or(0, |list| list.entries.len() as u32);
    // Decorated *after* the take, so the caller's status counter drops on this response rather
    // than on its next keystroke.
    let cursor = match params.buffer_id {
        Some(buffer_id) => {
            let current = s.cursors.get(&(client_id, buffer_id)).copied();
            current.map(|c| wrap_for_response(s, client_id, buffer_id, c))
        }
        None => None,
    };
    // Only when something actually went: clearing an already-empty list changes nothing for anyone.
    let pushes = if cleared > 0 {
        jumplist_changed_pushes(s, client_id)
    } else {
        Vec::new()
    };
    drop(guard);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    Ok(JumplistClearResult { cleared, cursor })
}

/// Step through the jumplist from the cursor's current location — Normal-mode `]` / `[`
/// (cross-file) or `Alt-]` / `Alt-[` (`CurrentFile` scope). Cursor-derived, stopping (not wrapping)
/// at the ends; the directional rules live in [`crate::jumplist::step_index`] (full) /
/// [`crate::jumplist::step_in_file`] (scoped). Returns [`JumplistStepResult::Empty`] when nothing
/// is captured, [`JumplistStepResult::AtEnd`] at the boundary in the step direction, and
/// [`JumplistStepResult::NoneInFile`] when a `CurrentFile` step finds no entries in the buffer's
/// file — each a no-op the client turns into a toast.
pub async fn jumplist_step(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: JumplistStepParams,
) -> Result<JumplistStepResult, RpcError> {
    let client_id = ctx.client_id;
    let (mut target, open_params, landing, idx) = {
        let s = state.lock().await;
        let here = step_location(&s, client_id, params.buffer_id)
            .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
        let Some(list) = s.jumplist(client_id) else {
            return Ok(JumplistStepResult::Empty);
        };
        if list.entries.is_empty() {
            return Ok(JumplistStepResult::Empty);
        }
        let location = here.as_location();
        // Use the outer edge of the cursor's selection so an entry the cursor currently sits
        // on is treated as "current" and skipped. Without this, `[` from a freshly-jumped
        // entry (where the selection covers its span) would land back on the same entry
        // because the entry's start position is < the cursor's end position.
        let edge = here.edge(params.direction);
        let count = params.count.max(1);
        let idx = match params.scope {
            JumplistStepScope::Full => match crate::jumplist::step_index(
                &list.entries,
                params.direction,
                location,
                edge,
                count,
            ) {
                Some(idx) => idx,
                // At the boundary in this direction — no move; client toasts "last/first entry".
                None => return Ok(JumplistStepResult::AtEnd),
            },
            JumplistStepScope::CurrentFile => match crate::jumplist::step_in_file(
                &list.entries,
                params.direction,
                location,
                edge,
                count,
            ) {
                crate::jumplist::InFileStep::Moved(idx) => idx,
                crate::jumplist::InFileStep::AtEnd => return Ok(JumplistStepResult::AtEnd),
                crate::jumplist::InFileStep::NoneInFile => {
                    return Ok(JumplistStepResult::NoneInFile)
                }
            },
        };
        let (target, open_params, landing) = step_plan(&s, client_id, &params, idx);
        (target, open_params, landing, idx)
    };
    let total = target.total;
    // Composite post-step. An entry captured from a view lands *in* that view: that is where the
    // row is, and its file is reached through the view, as one of the windows onto it.
    // `params.open` is the licence to reopen a view that has gone — without it a step still seats
    // in one already on screen, but conjures nothing back.
    let Some((view_key, identity, line)) = landing else {
        if let Some(open_params) = open_params {
            target.opened = Some(view_open(state, ctx, open_params).await?);
        }
        return Ok(JumplistStepResult::Moved(Box::new(target)));
    };
    let mut landing = land_in_captured_view(
        state,
        ctx,
        client_id,
        &view_key,
        &identity,
        line,
        params.open,
    )
    .await?;
    // An entry the view no longer holds is stepped **over**, or the key would stick on it: the
    // step is cursor-relative, and a landing that moved nothing leaves the next press choosing the
    // same entry again. The view is reached once; the entries after it are looked up in it directly.
    let mut skipped = 0u32;
    let mut reopened: Option<ViewOpenResult> = None;
    let mut idx = idx;
    loop {
        match landing {
            Landing::Seated {
                open,
                seat,
                position,
            } => {
                target.seat = Some(seat);
                target.opened = open.or(reopened);
                // The line as the element's buffer numbers it. Translated, the captured selection
                // means nothing.
                if target.position.map(|p| p.line) != Some(position.line) {
                    target.anchor = None;
                }
                target.position = Some(position);
                target.skipped = skipped;
                return Ok(JumplistStepResult::Moved(Box::new(target)));
            }
            Landing::NoView => return Ok(JumplistStepResult::Moved(Box::new(target))),
            Landing::Gone { open } => {
                skipped += 1;
                reopened = reopened.or(open);
                let next = match params.direction {
                    Direction::Forward => idx + 1,
                    Direction::Backward => idx.wrapping_sub(1),
                };
                let s = state.lock().await;
                if s.jumplist(client_id)
                    .is_none_or(|l| next >= l.entries.len())
                {
                    return Ok(JumplistStepResult::Gone {
                        index: idx as u32 + 1,
                        total,
                        skipped,
                        opened: reopened.map(Box::new),
                    });
                }
                idx = next;
                let (next_target, _, next_landing) = step_plan(&s, client_id, &params, idx);
                target = next_target;
                landing = match next_landing {
                    Some((key, identity, line)) if key == view_key => {
                        match crate::handlers::viewport::element_holding(
                            &s, client_id, &view_key, &identity, line,
                        )
                        .or_else(|| {
                            let view = reopened.as_ref()?.buffer_id;
                            crate::handlers::viewport::seat_in_fresh_view(&s, view, &identity, line)
                        }) {
                            Some((seat, position)) => Landing::Seated {
                                open: None,
                                seat,
                                position,
                            },
                            None => Landing::Gone { open: None },
                        }
                    }
                    // An entry of another view, or of a file: the walk continues through it in the
                    // ordinary way, without the reopened view — which the client is not shown.
                    Some((key, identity, line)) => {
                        drop(s);
                        land_in_captured_view(
                            state,
                            ctx,
                            client_id,
                            &key,
                            &identity,
                            line,
                            params.open,
                        )
                        .await?
                    }
                    None => {
                        let open_params = (params.open).then(|| {
                            jumplist_open_params(
                                &s,
                                client_id,
                                &s.jumplist(client_id).unwrap().entries[idx],
                                params.buffer_id,
                            )
                        });
                        drop(s);
                        if let Some(open_params) = open_params {
                            target.opened = Some(view_open(state, ctx, open_params).await?);
                        }
                        target.skipped = skipped;
                        return Ok(JumplistStepResult::Moved(Box::new(target)));
                    }
                };
            }
        }
    }
}

/// Where the client is, as a place among the jumplist's entries — see
/// [`crate::jumplist::StepLocation`]. `None` when the buffer is gone.
pub fn step_location(
    s: &ServerState,
    client_id: ClientId,
    buffer_id: BufferId,
) -> Option<crate::jumplist::StepLocation> {
    let doc = s.try_doc_of(buffer_id)?;
    let cursor = s
        .cursors
        .get(&(client_id, buffer_id))
        .copied()
        .unwrap_or_default();
    // A buffer with no view yet — an open decorating its cursor before its view is made — has no
    // pathless identity; `ViewId(0)` is never minted, so no entry matches it.
    let view =
        crate::handlers::viewport::client_view_of(s, client_id, buffer_id).unwrap_or_default();
    let plain = crate::jumplist::StepLocation {
        path: doc
            .canonical_path
            .as_deref()
            .map(|p| p.to_string_lossy().into_owned()),
        view,
        buffer: buffer_id,
        view_key: crate::handlers::viewport::buffer_view_key(s, buffer_id),
        cursor,
        translated: false,
    };
    let Some(generated) = doc.patch() else {
        return Some(plain);
    };
    // The patch document itself: the client's focused element windows generated text. Its place
    // is the outline entry the cursor is in, in that entry's file.
    let Some((_, entry)) = crate::handlers::viewport::outline_entry_at(s, client_id, buffer_id)
    else {
        return Some(plain);
    };
    let Some(identity) = entry.identity.clone() else {
        return Some(plain);
    };
    // A patch row's line in the file: the new side's, or — on a row with none, a removed line
    // or a header — the hunk's top.
    let file_line = |patch_line: u32| -> u32 {
        generated
            .index
            .lines
            .get(patch_line as usize)
            .copied()
            .flatten()
            .and_then(|info| info.new_lineno)
            .map_or(entry.file_lines.start, |n| n.saturating_sub(1))
    };
    let map = |p: LogicalPosition| LogicalPosition {
        line: file_line(p.line),
        col: p.col,
    };
    let (path, view_key) = if identity.starts_with('/') {
        (Some(identity), None)
    } else {
        (None, Some(identity))
    };
    Some(crate::jumplist::StepLocation {
        path,
        view,
        buffer: buffer_id,
        view_key,
        cursor: CursorState {
            position: map(cursor.position),
            anchor: map(cursor.anchor),
            ..cursor
        },
        translated: true,
    })
}

/// What landing on entry `idx` takes: how it names the view it came from (key, file identity, file
/// line), the target as the client sees it, and the open a file-shaped entry needs. Resolved after
/// the lock by `land_in_captured_view` — the same call the picker's own Enter makes, so the two
/// cannot disagree about where a row goes.
fn step_plan(
    s: &ServerState,
    client_id: ClientId,
    params: &JumplistStepParams,
    idx: usize,
) -> (
    JumplistStepTarget,
    Option<ViewOpenParams>,
    Option<(String, String, u32)>,
) {
    let list = s.jumplist(client_id).expect("a captured list");
    let entry = &list.entries[idx];
    // Which element of the view to land in, named the way the view itself names its elements'
    // buffers: a path for a working-tree file, a virtual key for a file at a revision.
    let landing = match (entry.view.clone(), entry.target.identity(), entry.position) {
        (Some(view), Some(identity), Some(position)) => {
            Some((view, identity.to_string(), position.line))
        }
        _ => None,
    };
    let target = JumplistStepTarget {
        path: entry.abs_path().map(str::to_string),
        view_id: entry.target.view_id(),
        position: entry.position,
        anchor: entry.anchor,
        index: idx as u32 + 1,
        total: list.entries.len() as u32,
        opened: None,
        seat: None,
        skipped: 0,
    };
    // Built whether or not it is used: it only describes the entry. Not for a view-addressed
    // entry: it names no file, and `jumplist_open_params` answers a pathless, id-less target with
    // a *scratch* buffer — which is what a commit-patch step opened once its dead buffer id stopped
    // erroring.
    let open_params = (params.open && landing.is_none() && entry.target.view_key().is_none())
        .then(|| jumplist_open_params(s, client_id, entry, params.buffer_id));
    (target, open_params, landing)
}
