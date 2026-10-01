//! `shell/*` — opening a shell view, running a command in it, and stopping one.
//!
//! The command text never travels: it is the input document's, which the server already holds, so
//! `shell/run` names only the view. What comes back travels as ordinary buffer content — the
//! transcript is a document and its output is `view/lines_changed` like any other change — and the
//! only thing this scope pushes of its own is *which command is running*, which is what the status
//! bar shows and what `shell/cancel` acts on.

use super::*;
use aether_protocol::shell::{
    NotRun, RunId, RunState, RunStatus, ShellCancelParams, ShellCancelResult, ShellRunChanged,
    ShellRunChangedParams, ShellRunParams, ShellRunResult, ShellStartParams, ShellStartResult,
};
use aether_protocol::ViewId;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How often a run's accumulated output is written into the transcript.
///
/// A compromise, and the number is the whole point: a build writes thousands of lines a second and
/// a document mutation costs a re-render on every viewport showing it, so writing per chunk would
/// spend the machine on repainting text nobody has read yet. Fifty milliseconds is under the
/// threshold at which output stops looking live.
const FLUSH_INTERVAL: Duration = Duration::from_millis(50);

/// Most lines one run may contribute before it is stopped. A runaway `yes` is the case; so is a
/// test suite that decides to print every assertion.
const MAX_LINES: u32 = 100_000;
/// Most bytes one run may contribute, whichever cap it reaches first. Sixteen mebibytes of text is
/// already far past what anyone will read, and well past what is comfortable to hold twice (rope
/// and wrap cache) per shell.
const MAX_BYTES: usize = 16 * 1024 * 1024;

/// What the transcript says when a run was stopped for producing too much.
const TRUNCATED: &str = "[output truncated — the run exceeded this shell's output limit]";

// ---- shell/start -------------------------------------------------------------------------------

pub async fn shell_start(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: ShellStartParams,
) -> Result<ShellStartResult, RpcError> {
    let cwd = match params.cwd {
        Some(dir) => {
            let dir = PathBuf::from(dir);
            if !dir.is_dir() {
                return Err(RpcError::invalid_params(format!(
                    "{} is not a directory",
                    crate::shell::display_path(&dir)
                )));
            }
            Some(dir)
        }
        None => None,
    };
    let line = params
        .input
        .as_deref()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string);
    let run = params.run && line.is_some();

    // The shell that last ran this line, here — if asked, and if there is one.
    let found = match (params.reuse, &cwd, &line) {
        (true, Some(dir), Some(line)) => {
            let s = state.lock().await;
            shell_for(&s, ctx.client_id, dir, line)
        }
        _ => None,
    };
    let (transcript, reused) = match found {
        Some(Found::Live(transcript)) => (transcript, true),
        // Brought back the way selecting its row would, which lands in it.
        Some(Found::Dormant(view_id)) => {
            let params = ViewOpenParams {
                view_id: Some(view_id),
                ..Default::default()
            };
            let opened = crate::handlers::buffer::view_open(state, ctx, params).await?;
            let s = state.lock().await;
            let transcript = s
                .try_presenting_buffer(opened.view_id)
                .ok_or_else(|| RpcError::internal("a restored shell has no view"))?;
            (transcript, true)
        }
        // Always a new one otherwise. Returning to a shell you already have is the shells
        // picker's job, which is a list you can see — unlike the reuse heuristic this
        // replaced, where the same key opened a new shell or an old one depending on which was
        // idle.
        None => {
            let fresh = Fresh {
                cwd,
                input: params.input,
            };
            (
                mint_shell(state, ctx.client_id, Start::Fresh(fresh)).await?,
                false,
            )
        }
    };
    // Not running it: the line goes in the input to be edited, replacing whatever was there — the
    // same edit a user's own select-all-and-type makes, so it is undoable like one.
    if let (true, false, Some(line)) = (reused, run, &line) {
        replace_input(state, ctx.client_id, transcript, line).await?;
    }
    let (mut opened, mut input) = land_in_input(state, ctx, transcript).await?;
    if !run {
        return Ok(ShellStartResult {
            opened,
            input,
            not_run: None,
        });
    }

    // A matched shell still running the line: switched to, and nothing more — stopping it is a
    // decision for the activity picker, not a side effect of asking again.
    let busy = {
        let s = state.lock().await;
        s.try_doc_of(transcript)
            .and_then(|d| d.transcript())
            .and_then(|t| Some(RpcError::shell_busy(&t.title, &t.active()?.command)))
    };
    let not_run = match busy {
        Some(e) => Some(NotRun::Busy { message: e.message }),
        // Through `shell/run`'s own path, so a line run on open is parsed, refused, recorded in the
        // recall list and started exactly as the same line typed and entered would be. A new
        // shell runs its input (landing first puts the caret where a refusal selects the word at
        // fault); a reused one runs the line itself, leaving whatever was typed in it alone.
        None => {
            let line = reused.then(|| line.clone()).flatten();
            run_line(state, ctx, opened.view_id, line)
                .await
                .err()
                .map(|e| NotRun::Refused { message: e.message })
        }
    };
    // A run is an element *above* the input, so the number the landing named the input by now
    // names the run. Viewports already showing the view follow the input by themselves
    // (`reinstate`); the answer has to be told, or the client subscribes focused on the run.
    let s = state.lock().await;
    if let Some(moved) = input_element_of(&s, transcript) {
        input = moved;
        if let Some(scroll) = opened.scroll.as_mut() {
            scroll.element = moved;
        }
    }
    Ok(ShellStartResult {
        opened,
        input,
        not_run,
    })
}

/// A shell `shell/start`'s `reuse` found.
enum Found {
    /// Open now, by its transcript.
    Live(BufferId),
    /// Restored from the session and not opened yet, by its row's view.
    Dormant(ViewId),
}

/// The first shell, in the shells picker's order, that is in `dir` and whose last command was
/// `line` — read off what the row shows: the directory the shell is in now (where the line would
/// run), and the last command, running or not. A restored shell answers from its snapshot's
/// summary, so finding one does not load it.
fn shell_for(
    s: &ServerState,
    client_id: ClientId,
    dir: &std::path::Path,
    line: &str,
) -> Option<Found> {
    let (live, dormant) = crate::handlers::picker::shell_rows(s, client_id);
    live.into_iter()
        .find_map(|view_id| {
            let transcript = s.try_view(view_id)?.presenting;
            let t = s.try_doc_of(transcript)?.transcript()?;
            (t.cwd == dir && t.runs.last().is_some_and(|r| r.command == line))
                .then_some(Found::Live(transcript))
        })
        .or_else(|| {
            dormant.into_iter().find_map(|d| {
                let Some(crate::state::DormantSummary::Shell(summary)) = &d.summary else {
                    return None;
                };
                (summary.cwd == dir && summary.last_command.as_deref() == Some(line))
                    .then_some(Found::Dormant(d.view))
            })
        })
}

/// Replace what is typed in `transcript`'s input with `line`, through the ordinary edit path.
async fn replace_input(
    state: &SharedState,
    client_id: ClientId,
    transcript: BufferId,
    line: &str,
) -> Result<(), RpcError> {
    let input = {
        let mut s = state.lock().await;
        let input = s
            .try_doc_of(transcript)
            .and_then(|d| d.transcript())
            .map(|t| t.input)
            .ok_or_else(|| RpcError::internal("a shell view has lost its input"))?;
        let end = motion::clamp_position(
            s.doc_of(input),
            LogicalPosition {
                line: u32::MAX,
                col: u32::MAX,
            },
        );
        set_cursor(
            &mut s,
            (client_id, input),
            CursorState {
                position: end,
                anchor: LogicalPosition { line: 0, col: 0 },
                match_bracket: None,
                jumplist_position: None,
            },
        );
        input
    };
    let empty = state.lock().await.doc_of(input).text.len_chars() == 0;
    let edit = EditKind::ReplaceWith {
        text: line.to_string(),
        select_pasted: false,
        // An empty input has no selection to replace — a point cursor would be a character.
        replace_selection: !empty,
        park_before: false,
    };
    apply_edit(state, client_id, input, edit).await?;
    Ok(())
}

/// How a shell comes to exist: new, or brought back from the snapshot written under its number.
enum Start {
    Fresh(Fresh),
    Restored(u32, crate::shell::ShellSnapshot),
}

/// What a new shell starts with beyond the defaults.
#[derive(Default)]
struct Fresh {
    /// Where it starts, in place of [`shell_cwd`]'s pick.
    cwd: Option<PathBuf>,
    /// What its input already says.
    input: Option<String>,
}

/// Open the shell `transcript` presents for this client and put the caret in its input.
///
/// Shared by a fresh open and a restore from a snapshot: the landing is the same whichever way
/// the shell came to exist.
async fn land_in_input(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    transcript: BufferId,
) -> Result<(ViewOpenResult, aether_protocol::ui::FieldId), RpcError> {
    let input_buffer = {
        let s = state.lock().await;
        s.try_doc_of(transcript)
            .and_then(|d| d.transcript())
            .map(|t| t.input)
            .ok_or_else(|| RpcError::internal("a shell view has lost its input"))?
    };
    land_in_field(state, ctx, transcript, input_buffer).await
}

/// Present `view_buffer` and put the caret at the end of `input_buffer`, its input element.
///
/// Shared by the shell and the agent views: both are composed views whose last element is a field
/// you type into, and the focus dance below is fiddly enough that a second copy of it would drift.
pub(crate) async fn land_in_field(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    view_buffer: BufferId,
    input_buffer: BufferId,
) -> Result<(ViewOpenResult, aether_protocol::ui::FieldId), RpcError> {
    let transcript = view_buffer;
    let client_id = ctx.client_id;
    let mut opened = present_buffer(state, ctx, transcript, OpenIntent::Navigate).await?;

    let mut s = state.lock().await;
    let input_element = input_element_of(&s, transcript)
        .ok_or_else(|| RpcError::internal("a composed view has lost its input"))?;
    // Land in the input. Two halves, because two different things read them: the *scroll* names
    // the element, and a fresh subscribe takes its focused element from the scroll — while a
    // viewport already showing this view needs telling directly, since it will re-subscribe with
    // the focus it already had.
    opened.scroll = Some(ScrollPosition {
        element: input_element,
        line: 0,
        sub_row: 0.0,
    });
    let view_id = opened.view_id;
    for vp in s.viewports.values_mut() {
        if vp.view_id == view_id && vp.client_id == client_id {
            vp.focused = input_element;
        }
    }
    // The cursor the client will find when focus lands there: the end of whatever was typed ahead,
    // which for a fresh shell is the start of an empty line.
    let end = motion::clamp_position(
        s.doc_of(input_buffer),
        LogicalPosition {
            line: u32::MAX,
            col: u32::MAX,
        },
    );
    set_cursor(
        &mut s,
        (client_id, input_buffer),
        CursorState {
            position: end,
            anchor: end,
            match_bracket: None,
            jumplist_position: None,
        },
    );
    Ok((opened, input_element))
}

/// A dormant shell, opened: read its snapshot back and build the shell the snapshot describes.
pub async fn open_restored_shell(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    number: u32,
) -> Result<ViewOpenResult, RpcError> {
    let client_id = ctx.client_id;
    let path = {
        let s = state.lock().await;
        let workspace = s.active_workspace_or_err(client_id)?.id.clone();
        s.backups_path
            .as_deref()
            .map(|root| crate::backup::shell_backup_path(root, &workspace, number))
    };
    let snapshot = path
        .and_then(|p| crate::backup::read(&p))
        .and_then(|(json, _)| serde_json::from_str::<crate::shell::ShellSnapshot>(&json).ok())
        .ok_or_else(|| {
            RpcError::new(
                aether_protocol::error::ErrorCode::BUFFER_NOT_FOUND,
                format!("Shell {number} has no snapshot to come back from"),
            )
        })?;
    let transcript = mint_shell(state, client_id, Start::Restored(number, snapshot)).await?;
    let (opened, _input) = land_in_input(state, ctx, transcript).await?;
    Ok(opened)
}

/// Create a shell: a transcript document, an input document, and the view over the two — fresh,
/// or as a snapshot left it.
async fn mint_shell(
    state: &SharedState,
    client_id: ClientId,
    start: Start,
) -> Result<BufferId, RpcError> {
    let (seed, fresh) = match start {
        Start::Fresh(fresh) => (None, fresh),
        Start::Restored(number, snapshot) => (Some((number, snapshot)), Fresh::default()),
    };
    let cwd = {
        let s = state.lock().await;
        s.active_workspace_or_err(client_id)?;
        match seed
            .as_ref()
            .map(|(_, snap)| snap.cwd.clone())
            .or(fresh.cwd)
        {
            // A directory that has gone since falls back to where a new shell would start.
            Some(dir) if dir.is_dir() => dir,
            _ => shell_cwd(&s, client_id),
        }
    };
    // The user's environment for this directory, exactly as `git_cli` resolves it — a shell that
    // can't find the tools the user's own shell would is a shell that can't build anything.
    // Resolved once here, outside the lock (it may run the login shell), and kept on the
    // transcript: from now on the shell's environment is its own.
    let env = crate::shell::environment(&cwd).await;

    let mut s = state.lock().await;
    let workspace = s.active_workspace_or_err(client_id)?.id.clone();
    let number = match &seed {
        Some((number, _)) => *number,
        None => s.next_shell_number(&workspace),
    };
    let title = format!("Shell {number}");

    // The input first: the transcript's own state names it, so it has to exist to be named. No
    // language: what is typed here is this shell's own command line, not anyone's script.
    let input = s.allocate_buffer_id();
    let typed = match &seed {
        Some((_, snap)) => Some(snap.input.clone()),
        None => fresh.input,
    };
    s.insert_buffer_with_document(input, None, false, |id| {
        let mut doc = Document::field(id, None);
        if let Some(typed) = typed.filter(|t| !t.is_empty()) {
            doc.restore_unsaved(&typed);
        }
        doc
    });
    s.buffer_workspaces.insert(input, workspace.clone());

    let transcript = s.allocate_buffer_id();
    let doc_id = s.allocate_document_id();
    let (t, text) = match seed {
        Some((_, mut snap)) => {
            // The directory the shell is in is the one decided above, which may not be the
            // snapshot's if that has gone.
            snap.cwd = cwd;
            crate::shell::Transcript::from_snapshot(input, title.clone(), snap, env)
        }
        None => {
            let mut t = crate::shell::Transcript::new(input, cwd, title.clone());
            t.env = env;
            (t, String::new())
        }
    };
    let doc = Document::virtual_content(
        doc_id,
        crate::state::VirtualSource {
            target: crate::state::VirtualTarget::shell(&workspace, number),
            title,
            commit: None,
        },
        text,
        None,
        Some(Generated::Shell(t)),
        false,
    );
    s.documents.insert(doc_id, doc);
    s.buffers.insert(
        transcript,
        Buffer {
            id: transcript,
            document: doc_id,
            scratch_number: None,
        },
    );
    s.buffer_workspaces.insert(transcript, workspace.clone());
    // Kept, explicitly. An open that says nothing is a preview, and a shell is somewhere you are
    // working, not a preview you glanced at — a transient one would close itself the moment you
    // looked at a file, taking a running build with it.
    s.open_view(transcript, Some(false));
    s.touch_mru(transcript);
    // Recorded in the session at once, as every other open is: the shell's snapshot is what
    // brings it back, and the session entry is what says there is one to bring. The `touch_mru`
    // above marked it dirty; the flush at the end of the request writes it.
    let pushes = refresh_view_pickers(&mut s);
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    Ok(transcript)
}

/// Where a new shell runs: the workspace root containing the file you were looking at, else the
/// workspace's first root.
///
/// Decided once, at open, and never revisited — a header that named a directory the commands are
/// no longer run in would be worse than no header at all.
pub(crate) fn shell_cwd(s: &ServerState, client_id: ClientId) -> PathBuf {
    let roots = s
        .active_workspace(client_id)
        .map(|w| w.paths.clone())
        .unwrap_or_default();
    let focused = s
        .viewports
        .values()
        .find(|v| v.client_id == client_id)
        .and_then(|v| s.try_doc_of(s.focused_buffer(v)))
        .and_then(|d| d.canonical_path.clone());
    if let Some(path) = focused {
        if let Some(root) = roots.iter().find(|root| path.starts_with(root)) {
            return root.clone();
        }
    }
    roots
        .first()
        .cloned()
        // A workspace with no roots at all (an ephemeral context that has not adopted one yet)
        // still has to run somewhere; the daemon's own directory is the only honest answer.
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Which element of a shell's view is its input.
fn input_element_of(s: &ServerState, transcript: BufferId) -> Option<aether_protocol::ui::FieldId> {
    s.try_view(s.view_presenting(transcript)?)?.input_element()
}

// ---- shell/run ---------------------------------------------------------------------------------

pub async fn shell_run(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: ShellRunParams,
) -> Result<ShellRunResult, RpcError> {
    run_line(state, ctx, params.view_id, None).await
}

/// Run a line in the shell `view_id` presents: `line`, or — `None` — what is typed in its input,
/// which is then cleared. A `line` given leaves the input alone, refusal included: the words a
/// refusal would select are not in it.
///
/// A line goes into the queue and [`advance`] starts it — at once when nothing is ahead of it,
/// otherwise when everything ahead has finished. Both halves go through the one path, so a line
/// run straight away and a line that waited cannot come to be started differently.
async fn run_line(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    view_id: ViewId,
    line: Option<String>,
) -> Result<ShellRunResult, RpcError> {
    let from_input = line.is_none();
    let client_id = ctx.client_id;
    let (transcript, input, source, busy, cwd, prev_cwd, env, workspace, current_file) = {
        let s = state.lock().await;
        let transcript = s
            .try_presenting_buffer(view_id)
            .ok_or_else(|| RpcError::view_not_found(view_id))?;
        let t = s
            .try_doc_of(transcript)
            .and_then(|d| d.transcript())
            .ok_or_else(|| RpcError::not_a_shell(view_id))?;
        let source = match line {
            Some(line) => line,
            None => s.doc_of(t.input).text.to_string(),
        };
        if source.trim().is_empty() {
            return Err(RpcError::invalid_params("nothing to run"));
        }
        (
            transcript,
            t.input,
            source,
            t.is_busy(),
            t.cwd.clone(),
            t.prev_cwd.clone(),
            t.env.clone(),
            s.active_workspace(client_id).and_then(|w| w.name.clone()),
            current_file(&s, transcript),
        )
    };

    // Parsed and checked against the world before anything is touched — and outside the lock,
    // because "is this on PATH" and "is that a directory" are filesystem questions.
    let checked = aether_shell::parse(&source).map(|program| {
        aether_shell::validate(
            &program,
            aether_shell::Context {
                cwd: &cwd,
                prev_cwd: prev_cwd.as_deref(),
                current_file: current_file.as_deref(),
            },
            &ServerWorld { env: &env },
        )
    });
    use aether_shell::Accepted;
    // A line that does not parse never will, so it is refused whatever is running. One that names
    // something not there is refused only when it would start now: behind a run, the thing it
    // names may be what that run makes, so a queued line is asked again when its turn comes. A
    // directory change is applied now either way — it runs nothing, so there is nothing to wait
    // for, and what you type next should already be where you moved to.
    let change_dir = match checked {
        Ok(Ok(Accepted::ChangeDir(dir))) => Some(dir),
        Ok(Ok(_)) => None,
        Ok(Err(_)) if busy => None,
        Err(refusal) | Ok(Err(refusal)) => {
            // The word at fault is selected so that typing replaces it; the text stays.
            if from_input {
                select_in_input(state, client_id, input, refusal.span).await;
            }
            return Err(RpcError::shell_rejected(refusal.message));
        }
    };
    let command = source.trim().to_string();

    // Clear the input through the ordinary edit path — selection then delete — rather than by
    // swapping its rope: that is what keeps the revision-guarded pushes and every viewer's cursor
    // coherent, and it is the same path a user pressing `Ctrl-a Delete` would take.
    if from_input {
        {
            let mut s = state.lock().await;
            let doc = s.doc_of(input);
            let end = motion::clamp_position(
                doc,
                LogicalPosition {
                    line: u32::MAX,
                    col: u32::MAX,
                },
            );
            set_cursor(
                &mut s,
                (client_id, input),
                CursorState {
                    position: end,
                    anchor: LogicalPosition { line: 0, col: 0 },
                    match_bracket: None,
                    jumplist_position: None,
                },
            );
        }
        apply_edit(state, client_id, input, EditKind::DeleteSelection).await?;
    }

    // Recorded only once it is really accepted, so a refused submit never enters the recall list.
    if let Some(workspace) = workspace {
        let mut s = state.lock().await;
        if s.history.record(
            &workspace,
            aether_protocol::history::HistoryKind::Shell,
            aether_protocol::history::HistoryEntry::bare(command.clone()),
        ) {
            s.history_dirty = true;
        }
    }

    // A directory change leaves no box: the directory in the input's title is where you now are,
    // and that is the whole of what happened.
    if let Some(dir) = change_dir {
        {
            let mut s = state.lock().await;
            s.with_transcript(transcript, |t| change_directory(t, dir));
        }
        // The rebuilt title is content, so it rides the ordinary content push.
        push_transcript_changed(state, transcript).await;
        return Ok(ShellRunResult { run: None });
    }

    let run = {
        let mut s = state.lock().await;
        s.with_transcript(transcript, |t| t.enqueue(command))
            .ok_or_else(|| RpcError::not_a_shell(view_id))?
    };
    // Waiting behind a run: its box is new content, and nothing else happens until its turn.
    if !advance(state.clone(), view_id, transcript).await {
        push_transcript_changed(state, transcript).await;
    }
    Ok(ShellRunResult { run: Some(run) })
}

/// Move the shell to `dir`, remembering where it was for `-`.
fn change_directory(t: &mut crate::shell::Transcript, dir: PathBuf) {
    let from = std::mem::replace(&mut t.cwd, dir);
    t.prev_cwd = Some(from);
}

/// What `%` names: the file most recently looked at in the shell's workspace — a real file, not a
/// patch, a revision or a shell. Asked of the workspace rather than of a client, because a queued
/// line asks it when its turn comes, and nobody pressed anything then.
fn current_file(s: &ServerState, transcript: BufferId) -> Option<PathBuf> {
    let workspace = s.buffer_workspaces.get(&transcript)?;
    let entry = s.workspaces.get(workspace)?;
    entry.mru_views.iter().find_map(|v| {
        let buffer = s.try_view(*v)?.presenting;
        let doc = s.try_doc_of(buffer)?;
        if doc.read_only() {
            return None;
        }
        doc.canonical_path.clone()
    })
}

/// Start what is next in `transcript`'s queue, if nothing is running — and, for a line that is over
/// the moment it starts, the one after it, until a process is going or the queue is empty.
/// Answers whether it started anything, having pushed whatever that changed.
///
/// Called after a line is queued and after a run finishes, which between them are every moment
/// the answer can change. The line is checked against the world **here**, when it starts, rather
/// than when it was typed: behind a build, the file it names may be one the build writes. One that
/// is refused now becomes a run that says why, and the queue carries on past it.
///
/// Boxed with its `Send` spelled out, because it and [`run_task`] start each other.
fn advance(
    state: SharedState,
    view_id: ViewId,
    transcript: BufferId,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>> {
    Box::pin(async move {
        let mut started = false;
        loop {
            let (handle, token) = crate::process::cancel_channel();
            let begun = {
                let mut s = state.lock().await;
                let ready = s
                    .try_doc_of(transcript)
                    .and_then(|d| d.transcript())
                    .is_some_and(|t| t.active().is_none() && !t.queue.is_empty());
                if !ready {
                    return started;
                }
                let start_char = s.doc_of(transcript).text.len_chars();
                let start_line = s.doc_of(transcript).content_lines();
                let current_file = current_file(&s, transcript);
                let begun = s
                    .with_transcript(transcript, |t| {
                        let (id, command, cwd) = {
                            let run = t.start_next(start_line, handle)?;
                            (run.id, run.command.clone(), run.cwd.clone())
                        };
                        Some((id, command, cwd, t.prev_cwd.clone(), t.env.clone()))
                    })
                    .flatten();
                // Registered as outstanding work, so a caller can wait for the server to go quiet
                // rather than guess at how long a command takes. Held for the run's whole life by
                // the task below.
                begun.map(|b| (b, current_file, start_char, s.deferred.start()))
            };
            let Some(((run, command, cwd, prev_cwd, env), current_file, start_char, deferred)) =
                begun
            else {
                return started;
            };
            started = true;

            let checked = aether_shell::parse(&command).and_then(|program| {
                aether_shell::validate(
                    &program,
                    aether_shell::Context {
                        cwd: &cwd,
                        prev_cwd: prev_cwd.as_deref(),
                        current_file: current_file.as_deref(),
                    },
                    &ServerWorld { env: &env },
                )
            });
            // Stopped in the moment it took to check: whatever it was, it does not happen.
            let cancelled = *token.borrow();
            use aether_shell::Accepted;
            match checked {
                Ok(Accepted::Run(plan)) => {
                    let running = {
                        let s = state.lock().await;
                        s.try_doc_of(transcript)
                            .and_then(|d| d.transcript())
                            .and_then(|t| t.run(run))
                            .map(|r| r.state())
                    };
                    // The run's box is new content — a new element, its command row — so it rides
                    // the content push now. Left to the output flush, a command that prints
                    // nothing (`sleep`, a quiet build step) would not appear until it had finished.
                    push_transcript_changed(&state, transcript).await;
                    push_run_changed(&state, view_id, running).await;
                    tokio::spawn(run_task(
                        state.clone(),
                        view_id,
                        transcript,
                        run,
                        plan,
                        cwd,
                        env,
                        token,
                        start_char,
                        deferred,
                    ));
                    return true;
                }
                // Reached only by a line that named a directory not there when it was queued: it
                // moves the shell as one typed now would, and leaves no box — the run it started
                // as said nothing, so it owns no line and goes without disturbing any other's.
                Ok(Accepted::ChangeDir(dir)) => {
                    {
                        let mut s = state.lock().await;
                        s.with_transcript(transcript, |t| {
                            t.runs.retain(|r| r.id != run);
                            if !cancelled {
                                change_directory(t, dir);
                            }
                        });
                    }
                    push_transcript_changed(&state, transcript).await;
                }
                // An assignment is a run like any other — a box, with nothing to say — so the
                // transcript records that it was made. Over the moment it starts.
                Ok(Accepted::Assign(vars)) => {
                    let status = if cancelled {
                        RunStatus::Cancelled
                    } else {
                        let mut s = state.lock().await;
                        s.with_transcript(transcript, |t| {
                            for (name, value) in vars {
                                t.assign(name, value);
                            }
                        });
                        RunStatus::Exited { code: 0 }
                    };
                    let finished = finish(&state, transcript, run, status, 0).await;
                    push_transcript_changed(&state, transcript).await;
                    push_run_changed(&state, view_id, finished).await;
                }
                Err(refusal) => {
                    {
                        let mut s = state.lock().await;
                        let text = format!("{}\n", refusal.message);
                        s.extend_transcript(transcript, start_char, &text);
                    }
                    let finished = finish(&state, transcript, run, RunStatus::Refused, 0).await;
                    push_transcript_changed(&state, transcript).await;
                    push_run_changed(&state, view_id, finished).await;
                }
            }
            drop(deferred);
        }
    })
}

/// Record how run `run` ended, and answer its final state.
async fn finish(
    state: &SharedState,
    transcript: BufferId,
    run: RunId,
    status: RunStatus,
    elapsed_ms: u64,
) -> Option<RunState> {
    let mut s = state.lock().await;
    s.with_transcript(transcript, |t| {
        let r = t.run_mut(run)?;
        r.status = status;
        r.elapsed_ms = Some(elapsed_ms);
        r.cancel = None;
        Some(r.state())
    })
    .flatten()
}

/// Select `span` (a byte range of the input's text) for `client_id`, and tell it so.
///
/// A refusal points at a word; selecting it is what lets the next keystroke replace it. The
/// push is the same content push an edit sends, which is how the client learns where its
/// cursor now is.
async fn select_in_input(
    state: &SharedState,
    client_id: ClientId,
    input: BufferId,
    span: aether_shell::Span,
) {
    if span.is_empty() {
        return;
    }
    let pushes = {
        let mut s = state.lock().await;
        let Some(doc) = s.try_doc_of(input) else {
            return;
        };
        let text = &doc.text;
        let start = span.start.min(text.len_bytes());
        let end = span.end.min(text.len_bytes());
        let position = |byte: usize| {
            let ch = text.byte_to_char(byte);
            let line = text.char_to_line(ch);
            let line_start = text.char_to_byte(text.line_to_char(line));
            LogicalPosition {
                line: line as u32,
                col: (byte - line_start) as u32,
            }
        };
        let anchor = position(start);
        // Selections are inclusive on both ends: the cursor sits on the range's last character.
        let last = text.char_to_byte(
            text.byte_to_char(end)
                .saturating_sub(1)
                .max(text.byte_to_char(start)),
        );
        let cursor = position(last);
        set_cursor(
            &mut s,
            (client_id, input),
            CursorState {
                position: cursor,
                anchor,
                match_bracket: None,
                jumplist_position: None,
            },
        );
        collect_doc_lines_changed_pushes(&s, input)
    };
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
}

/// The world as the shell sees it: its own environment, and the filesystem.
struct ServerWorld<'a> {
    env: &'a std::collections::HashMap<String, String>,
}

impl aether_shell::World for ServerWorld<'_> {
    fn variable(&self, name: &str) -> Option<String> {
        self.env.get(name).cloned()
    }

    fn path_kind(&self, path: &std::path::Path) -> Option<aether_shell::PathKind> {
        use std::os::unix::fs::PermissionsExt;
        let md = std::fs::metadata(path).ok()?;
        Some(if md.is_dir() {
            aether_shell::PathKind::Dir
        } else if md.is_file() {
            aether_shell::PathKind::File {
                executable: md.permissions().mode() & 0o111 != 0,
            }
        } else {
            aether_shell::PathKind::Other
        })
    }

    fn list_dir(&self, dir: &std::path::Path) -> Vec<(String, aether_shell::PathKind)> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|e| {
                let kind = self.path_kind(&e.path())?;
                Some((e.file_name().to_string_lossy().into_owned(), kind))
            })
            .collect()
    }
}

// ---- shell/cancel ------------------------------------------------------------------------------

pub async fn shell_cancel(
    state: &SharedState,
    _ctx: &mut ConnectionCtx,
    params: ShellCancelParams,
) -> Result<ShellCancelResult, RpcError> {
    let mut s = state.lock().await;
    let transcript = s
        .try_presenting_buffer(params.view_id)
        .ok_or_else(|| RpcError::view_not_found(params.view_id))?;
    if s.try_doc_of(transcript)
        .and_then(|d| d.transcript())
        .is_none()
    {
        return Err(RpcError::not_a_shell(params.view_id));
    }
    // Taking the handle rather than merely signalling through it: a second cancel after the
    // run has ended must not signal a pid the system has since reused.
    let cancelled = s
        .with_transcript(transcript, |t| {
            t.active_mut()
                .and_then(|run| run.cancel.take())
                .is_some_and(|handle| handle.send(true).is_ok())
        })
        .unwrap_or(false);
    Ok(ShellCancelResult { cancelled })
}

/// Stop run `run` of the shell `view_id` presents if it is going, or take it out of the queue if it
/// is waiting — what a run's cancel button does. A running one ends exactly as `shell/cancel` ends
/// it, killed and kept; a queued one leaves no trace, since nothing happened. False when it is
/// neither any more.
pub async fn shell_cancel_run(
    state: &SharedState,
    view_id: ViewId,
    run: RunId,
) -> Result<bool, RpcError> {
    let unqueued = {
        let mut s = state.lock().await;
        let transcript = s
            .try_presenting_buffer(view_id)
            .ok_or_else(|| RpcError::view_not_found(view_id))?;
        let t = s
            .try_doc_of(transcript)
            .and_then(|d| d.transcript())
            .ok_or_else(|| RpcError::not_a_shell(view_id))?;
        if t.queue.iter().any(|q| q.id == run) {
            s.with_transcript(transcript, |t| t.unqueue(run));
            Some(transcript)
        } else if t.active().is_some_and(|r| r.id == run) {
            // The same stop the activity picker's makes, so the two cannot end a run differently.
            return Ok(s
                .with_transcript(transcript, |t| {
                    t.active_mut()
                        .and_then(|r| r.cancel.take())
                        .is_some_and(|handle| handle.send(true).is_ok())
                })
                .unwrap_or(false));
        } else {
            None
        }
    };
    match unqueued {
        Some(transcript) => {
            push_transcript_changed(state, transcript).await;
            Ok(true)
        }
        None => Ok(false),
    }
}

// ---- running -----------------------------------------------------------------------------------

/// Run one accepted line and stream its output into the transcript.
///
/// Two halves that cannot be one: [`crate::process::run_pipeline`] hands over chunks from a
/// synchronous callback, and writing them into a document needs the state lock. So the callback
/// only forwards (and counts, for the cap) while this task drains, coalescing on a timer.
#[allow(clippy::too_many_arguments)]
async fn run_task(
    state: SharedState,
    view_id: ViewId,
    transcript: BufferId,
    run: RunId,
    plan: aether_shell::Plan,
    cwd: PathBuf,
    env: std::collections::HashMap<String, String>,
    token: crate::process::CancelToken,
    start_char: usize,
    // Dropped when the run ends, which is what makes "wait for the server to go quiet" cover a
    // command still producing output.
    _deferred: DeferredToken,
) {
    let started = Instant::now();
    let token_seen = token.clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let capped = Arc::new(AtomicBool::new(false));
    let runner = tokio::spawn({
        let capped = capped.clone();
        async move {
            let mut bytes = 0usize;
            let mut lines = 0u32;
            let mut on_chunk = move |chunk: &[u8]| {
                bytes += chunk.len();
                lines += chunk.iter().filter(|b| **b == b'\n').count() as u32;
                let _ = tx.send(chunk.to_vec());
                if bytes > MAX_BYTES || lines > MAX_LINES {
                    capped.store(true, Ordering::Relaxed);
                    return false;
                }
                true
            };
            execute_plan(plan, &cwd, &env, token, &mut on_chunk).await
        }
    });
    // Kept to ask afterwards whether the stop was ours: a cancel reads as one however the command
    // took the signal — killed by it, or exiting with a code of its own on the way out.
    let asked_to_stop = token_seen;

    let mut out = crate::process::OutputText::default();
    let mut flushed = Flushed::default();
    let mut ticker = tokio::time::interval(FLUSH_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            // Biased with the timer first so a command producing output as fast as it can be read
            // still yields a flush every interval rather than starving one indefinitely.
            biased;
            _ = ticker.tick() => flush(&state, transcript, start_char, &out, &mut flushed).await,
            chunk = rx.recv() => match chunk {
                Some(chunk) => out.push(&chunk),
                None => break,
            },
        }
    }

    let code = runner.await.ok().flatten();
    let capped = capped.load(Ordering::Relaxed);
    // A run that said something ends on a newline, so the next one starts on a fresh line. A run
    // that said nothing owns no line at all: its box is its title and its command, and the view
    // draws no output row for it.
    if out.bytes() > 0 {
        out.end_line();
    }
    if capped {
        out.push_line(TRUNCATED);
    }
    flush(&state, transcript, start_char, &out, &mut flushed).await;

    let status = match (capped, *asked_to_stop.borrow(), code) {
        (true, _, _) => RunStatus::Truncated,
        // Stopped because someone asked — `shell/cancel`, the stop button, the view closing.
        (false, true, _) => RunStatus::Cancelled,
        (false, false, Some(code)) => RunStatus::Exited { code },
        // No code and nobody here asked: a signal from outside, which is a failure.
        (false, false, None) => RunStatus::Killed,
    };
    let elapsed = started.elapsed().as_millis() as u64;
    let finished = finish(&state, transcript, run, status, elapsed).await;
    // The rebuilt header is content, so it rides the ordinary content push; the run state rides
    // its own, for the status indicator and the toast.
    push_transcript_changed(&state, transcript).await;
    push_run_changed(&state, view_id, finished).await;
    // Whatever was waiting on this one goes next, however this one went. Before `_deferred` is
    // dropped, so a server waiting to go quiet does not see a gap between the two.
    advance(state, view_id, transcript).await;
}

/// Run a plan's pipelines in order, honouring `&&` and `||`, and answer the line's exit code —
/// `None` when it was killed.
///
/// A pipeline's code is its **first failing stage's**, not its last's, so `false | cat` is not
/// a success. A list's code is the last pipeline that ran. A runtime failure — a program that
/// cannot be spawned, a redirection that cannot be opened, a directory that has gone — is
/// written into the transcript as the run's output and reported as a failure, because the line
/// was accepted and what became of it is the record.
async fn execute_plan(
    plan: aether_shell::Plan,
    cwd: &std::path::Path,
    env: &std::collections::HashMap<String, String>,
    token: crate::process::CancelToken,
    on_chunk: &mut (impl FnMut(&[u8]) -> bool + Send),
) -> Option<i32> {
    use aether_shell::{Exec, ListOp};
    if !cwd.is_dir() {
        on_chunk(format!("working directory no longer exists: {}\n", cwd.display()).as_bytes());
        return Some(1);
    }
    let mut last: Option<i32> = Some(0);
    for item in plan.items {
        let run_it = match item.op {
            None | Some(ListOp::Seq) => true,
            Some(ListOp::And) => last == Some(0),
            Some(ListOp::Or) => last != Some(0),
        };
        if !run_it {
            continue;
        }
        if *token.borrow() {
            return None;
        }
        let code = match item.stages.as_slice() {
            [stage] if matches!(stage.exec, Exec::Builtin(_)) => {
                run_builtin(stage, cwd, env, on_chunk)
            }
            stages => {
                let specs = stage_specs(stages);
                let mut sink = |_stream, chunk: &[u8]| on_chunk(chunk);
                match crate::process::run_pipeline(specs, cwd, env, token.clone(), &mut sink).await
                {
                    Ok(codes) => {
                        if codes.iter().any(Option::is_none) {
                            None
                        } else {
                            codes.into_iter().flatten().find(|c| *c != 0).or(Some(0))
                        }
                    }
                    Err(e) => {
                        on_chunk(format!("{e}\n").as_bytes());
                        Some(if e.kind() == std::io::ErrorKind::NotFound {
                            127
                        } else {
                            1
                        })
                    }
                }
            }
        };
        // A killed pipeline ends the line: nothing after a cancel should start.
        last = Some(code?);
    }
    last
}

/// A pipeline's stages as the process module wants them: each stage's stdin is the previous
/// stage's stdout unless a `<` says otherwise, and its stdout is the next stage's stdin, or the
/// transcript for the last, unless a `>` or `>>` says otherwise. The last redirection of a kind
/// wins, as it does everywhere.
fn stage_specs(stages: &[aether_shell::Stage]) -> Vec<crate::process::StageSpec> {
    use crate::process::{StageSpec, StageStdin, StageStdout};
    use aether_shell::{Exec, RedirectKind};
    let last = stages.len().saturating_sub(1);
    stages
        .iter()
        .enumerate()
        .map(|(i, stage)| {
            let program = match &stage.exec {
                Exec::Path(p) => p.clone(),
                // Refused by the validator inside a pipeline; never reached.
                Exec::Builtin(b) => PathBuf::from(b.name()),
            };
            let mut stdin = if i == 0 {
                StageStdin::Null
            } else {
                StageStdin::Pipe
            };
            let mut stdout = if i == last {
                StageStdout::Sink
            } else {
                StageStdout::Pipe
            };
            for r in &stage.redirects {
                match r.kind {
                    RedirectKind::In => stdin = StageStdin::File(r.path.clone()),
                    RedirectKind::Out => {
                        stdout = StageStdout::File {
                            path: r.path.clone(),
                            append: false,
                        }
                    }
                    RedirectKind::Append => {
                        stdout = StageStdout::File {
                            path: r.path.clone(),
                            append: true,
                        }
                    }
                }
            }
            StageSpec {
                program,
                args: stage.argv.iter().skip(1).cloned().collect(),
                env: stage.env.clone(),
                stdin,
                stdout,
            }
        })
        .collect()
}

/// The commands the shell answers itself.
fn run_builtin(
    stage: &aether_shell::Stage,
    cwd: &std::path::Path,
    env: &std::collections::HashMap<String, String>,
    on_chunk: &mut (impl FnMut(&[u8]) -> bool + Send),
) -> Option<i32> {
    use aether_shell::{Builtin, Exec};
    let Exec::Builtin(builtin) = stage.exec else {
        return Some(0);
    };
    let mut out = String::new();
    let mut code = 0;
    match builtin {
        Builtin::Pwd => {
            out.push_str(&cwd.display().to_string());
            out.push('\n');
        }
        Builtin::Type => {
            let world = ServerWorld { env };
            for name in stage.argv.iter().skip(1) {
                if Builtin::named(name).is_some() {
                    out.push_str(&format!("{name} is a shell builtin\n"));
                } else if let Some(path) = aether_shell::find_executable(name, &world) {
                    out.push_str(&format!("{name} is {}\n", path.display()));
                } else if name.contains('/')
                    && matches!(
                        aether_shell::World::path_kind(&world, &cwd.join(name)),
                        Some(aether_shell::PathKind::File { executable: true })
                    )
                {
                    out.push_str(&format!("{name} is {}\n", cwd.join(name).display()));
                } else {
                    out.push_str(&format!("{name}: not found\n"));
                    code = 1;
                }
            }
        }
    }
    on_chunk(out.as_bytes());
    Some(code)
}

/// How much of a run's output is already in the document, and where the rewritable tail starts.
#[derive(Default)]
struct Flushed {
    /// Bytes of the run's text the last flush wrote.
    bytes: usize,
    /// Byte offset in the run's text of the line the last flush left open — the point the next one
    /// rewrites from. See [`crate::process::OutputText::line_start`].
    line_start: usize,
    /// The same point as a **char** offset, carried forward so a flush never has to count the
    /// characters of everything written so far.
    line_start_chars: usize,
}

/// Write everything since the last flush into the transcript, and push it to whoever is watching.
async fn flush(
    state: &SharedState,
    transcript: BufferId,
    start_char: usize,
    out: &crate::process::OutputText,
    flushed: &mut Flushed,
) {
    if out.bytes() == flushed.bytes {
        return;
    }
    let text = out.text();
    let from = flushed.line_start;
    let tail = &text[from..];
    {
        let mut s = state.lock().await;
        if !s.extend_transcript(transcript, start_char + flushed.line_start_chars, tail) {
            return; // the shell closed under us
        }
    }
    // Everything up to the *new* open line is final; count only the characters between the old
    // rewrite point and the new one, which is proportional to what just arrived rather than to
    // everything the run has ever said.
    let line_start = out.line_start();
    flushed.line_start_chars += text[from..line_start].chars().count();
    flushed.line_start = line_start;
    flushed.bytes = text.len();
    push_transcript_changed(state, transcript).await;
}

/// The content push for a transcript that just grew — the same one an edit sends, because to
/// everything downstream this *is* an edit to a document somebody is viewing.
async fn push_transcript_changed(state: &SharedState, transcript: BufferId) {
    let pushes = {
        let s = state.lock().await;
        collect_doc_lines_changed_pushes(&s, transcript)
    };
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
}

/// Push `shell/run_changed` to every client on the shell's workspace, re-push every open shells
/// picker, and tell every client its workspace's work in progress ([`push_activity`]).
///
/// Every client on the workspace: a shell belongs to the workspace rather than to whoever pressed
/// `Enter`, and a client not showing it wants to hear how the run went whether or not it started
/// it. A client on another workspace has no row for it to belong to.
///
/// The picker re-push rides here because this is the one funnel a run transition passes through,
/// and a row's badge (`● running` → `✓ 0  3.2s`) is exactly what just changed. Recency ordering
/// means the row re-paints where it is.
async fn push_run_changed(state: &SharedState, view_id: ViewId, run: Option<RunState>) {
    let picker_pushes = {
        let mut s = state.lock().await;
        refresh_shell_pickers(&mut s)
    };
    for (sender, notif) in picker_pushes {
        let _ = sender.send(notif).await;
    }
    let pushes: PendingPushes = {
        let s = state.lock().await;
        let title = s
            .try_presenting_buffer(view_id)
            .and_then(|b| s.try_doc_of(b))
            .and_then(|d| d.transcript())
            .map(|t| t.title.clone())
            .unwrap_or_default();
        let params = ShellRunChangedParams {
            view_id,
            title,
            run,
        };
        let value = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
        clients_of_view(&s, view_id)
            .into_iter()
            .map(|sender| {
                (
                    sender,
                    Notification {
                        jsonrpc: JsonRpc,
                        method: ShellRunChanged::NAME.into(),
                        params: value.clone(),
                    },
                )
            })
            .collect()
    };
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    push_activity(state).await;
}

// ---- view/follow_line ----------------------------------------------------------------------------

/// `Enter` in a composed view: follow the line under the cursor to whatever it names.
///
/// **Total** over the kinds of generated content, which is the point: the client asks one question
/// of every view it did not open from a file, and the answer is decided here by what the document
/// actually is rather than by a flag the client carries. A view with no generated content answers
/// `None` rather than erroring, so a stale route costs nothing.
pub async fn view_follow_line(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: aether_protocol::view::ViewFollowLineParams,
) -> Result<aether_protocol::view::ViewFollowLineResult, RpcError> {
    use aether_protocol::view::ViewFollowLineResult;

    let client_id = ctx.client_id;
    // Resolved under one short lock; the open below takes the lock itself.
    enum Follow {
        Patch(BufferId),
        File(std::path::PathBuf, LogicalPosition),
        Nothing,
    }
    let follow = {
        let s = state.lock().await;
        let Some(view_buffer) = s.try_presenting_buffer(params.view_id) else {
            return Ok(ViewFollowLineResult { opened: None });
        };
        // The buffer the cursor is actually in — the focused element's, which for a bound element
        // is a real file and for a generated one is the view's own document.
        let focused = s
            .viewports
            .values()
            .find(|v| v.client_id == client_id && v.view_id == params.view_id)
            .map(|v| s.focused_buffer(v))
            .unwrap_or(view_buffer);
        let line = s
            .cursors
            .get(&(client_id, focused))
            .map_or(0, |c| c.position.line);
        match s.try_doc_of(view_buffer).and_then(|d| d.generated.as_ref()) {
            // The patch's own index says where a line came from; that logic stays where it is.
            Some(Generated::Patch(_)) => Follow::Patch(view_buffer),
            Some(Generated::Shell(t)) => {
                let doc = s.doc_of(focused);
                let text: String = doc
                    .text
                    .get_line(line as usize)
                    .map(|l| l.chars().collect())
                    .unwrap_or_default();
                match crate::shell::parse_location(&text)
                    .and_then(|loc| crate::shell::resolve(&loc, &t.cwd).map(|p| (loc, p)))
                {
                    Some((loc, path)) => Follow::File(
                        path,
                        LogicalPosition {
                            line: loc.line,
                            col: loc.col,
                        },
                    ),
                    None => Follow::Nothing,
                }
            }
            // A conversation answers the same question two ways, in this order: a tool call that
            // told us where it was working is followed to *there*, which is the whole reason ACP
            // sends locations; otherwise the line is read for a `path:line:col` exactly as a
            // shell's output is, so an agent that merely printed a compiler error is still
            // followable.
            Some(Generated::Agent(c)) => {
                let located = c
                    .blocks
                    .iter()
                    .find(|b| b.buffer == focused)
                    .and_then(|b| match &b.kind {
                        crate::agent::BlockKind::ToolCall(tc) => tc.locations.first(),
                        _ => None,
                    })
                    .map(|loc| {
                        (
                            loc.path.clone(),
                            LogicalPosition {
                                line: loc.line.unwrap_or(0),
                                col: 0,
                            },
                        )
                    });
                match located {
                    Some((path, position)) => Follow::File(path, position),
                    None => {
                        let doc = s.doc_of(focused);
                        let text: String = doc
                            .text
                            .get_line(line as usize)
                            .map(|l| l.chars().collect())
                            .unwrap_or_default();
                        match crate::shell::parse_location(&text)
                            .and_then(|loc| crate::shell::resolve(&loc, &c.cwd).map(|p| (loc, p)))
                        {
                            Some((loc, path)) => Follow::File(
                                path,
                                LogicalPosition {
                                    line: loc.line,
                                    col: loc.col,
                                },
                            ),
                            None => Follow::Nothing,
                        }
                    }
                }
            }
            None => Follow::Nothing,
        }
    };

    match follow {
        Follow::Nothing => Ok(ViewFollowLineResult { opened: None }),
        Follow::Patch(buffer_id) => {
            let followed = git_follow_patch_line(
                state,
                ctx,
                aether_protocol::git::GitFollowPatchLineParams { buffer_id },
            )
            .await?;
            Ok(ViewFollowLineResult {
                opened: followed.opened,
            })
        }
        // An ordinary open, saying nothing about keeping: following an output line is a glance at
        // where it points, so the file arrives as a preview and stays one until you do something
        // to it — the same rule `Enter` on a patch's bound element follows. A file already kept is
        // never demoted by this. `record_nav_from` is the view, so `Alt-Left` comes back here.
        Follow::File(path, jump_to) => {
            let view_buffer = {
                let s = state.lock().await;
                s.try_presenting_buffer(params.view_id)
            };
            let opened = view_open(
                state,
                ctx,
                ViewOpenParams {
                    absolute_path: Some(path.to_string_lossy().into_owned()),
                    jump_to: Some(jump_to),
                    record_nav_from: view_buffer,
                    ..Default::default()
                },
            )
            .await?;
            Ok(ViewFollowLineResult {
                opened: Some(opened),
            })
        }
    }
}

// ---- view/submit_input -------------------------------------------------------------------------

/// **Total** over the kinds of composed view: what `Enter` in an input element means here.
///
/// The client cannot tell a shell from an agent view — the window marks the input by role and
/// carries no kind — so it asks this and the server decides, the same arrangement
/// [`view_follow_line`] uses for `Enter` on a line. A view with no input, or one whose kind has no
/// notion of submitting, answers `submitted: false`.
pub async fn view_submit_input(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: aether_protocol::view::ViewSubmitInputParams,
) -> Result<aether_protocol::view::ViewSubmitInputResult, RpcError> {
    use aether_protocol::history::HistoryKind;
    use aether_protocol::view::ViewSubmitInputResult;

    enum Submit {
        Shell,
        Agent,
        Nothing,
    }
    let submit = {
        let s = state.lock().await;
        let Some(view_buffer) = s.try_presenting_buffer(params.view_id) else {
            return Ok(ViewSubmitInputResult::default());
        };
        match s.try_doc_of(view_buffer).and_then(|d| d.generated.as_ref()) {
            Some(Generated::Shell(_)) => Submit::Shell,
            Some(Generated::Agent(_)) => Submit::Agent,
            Some(Generated::Patch(_)) | None => Submit::Nothing,
        }
    };

    match submit {
        Submit::Nothing => Ok(ViewSubmitInputResult::default()),
        Submit::Shell => {
            // A directory change runs nothing but is still a submission: the line left the input
            // and belongs in the recall list, which is why the run id is not what decides this.
            shell_run(
                state,
                ctx,
                ShellRunParams {
                    view_id: params.view_id,
                },
            )
            .await?;
            Ok(ViewSubmitInputResult {
                submitted: true,
                history: Some(HistoryKind::Shell),
            })
        }
        Submit::Agent => {
            let sent = crate::handlers::agent_prompt(
                state,
                ctx,
                aether_protocol::agent::AgentPromptParams {
                    view_id: params.view_id,
                },
            )
            .await?;
            Ok(ViewSubmitInputResult {
                submitted: sent.sent,
                history: sent.sent.then_some(HistoryKind::Agent),
            })
        }
    }
}
