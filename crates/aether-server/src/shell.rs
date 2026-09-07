//! What a shell view *is*, server-side: a transcript of runs and the input under it.
//!
//! The transcript is a virtual document — read-only by construction, like a patch — whose text is
//! the concatenated output of the shell's runs. Each run is one element of the view, in a box of
//! its own named for where it ran and — once it is over — how it went and how long it took, with
//! the command on the row inside. The input is an ordinary editable document bound as the view's
//! **last** element, which is the same mechanism the working-changes view uses to bind real files
//! into a composed view.
//!
//! This module holds the model and the box's name; [`crate::handlers::shell`] runs the commands and
//! [`crate::state`] owns the document mutations. The split is the usual one: nothing here touches
//! a rope, and nothing here spawns a process.

use aether_protocol::shell::{RunId, RunStatus};
use aether_protocol::viewport::Element;
use aether_protocol::BufferId;
use std::path::{Path, PathBuf};

/// One shell view's state.
///
/// Lives on the transcript document (as `Generated::Shell`), because it *is* what that document's
/// content was generated from — the same place a patch's index lives, for the same reason: the
/// text and the account of what the text means are built together and must not drift apart.
#[derive(Debug)]
pub struct Transcript {
    /// The runs, oldest first. One element of the view each.
    pub runs: Vec<Run>,
    /// The document holding the command being typed. Internal: never listed, never backed up,
    /// never session-recorded, never dirty (see `Document::internal`). Dropped with the view.
    pub input: BufferId,
    /// Where the next command runs. Set when the shell was opened and moved by a directory
    /// change; every run records the directory it ran in on its own box, so this moving never
    /// makes an old box lie.
    pub cwd: PathBuf,
    /// Where `-` goes: the directory before the last change.
    pub prev_cwd: Option<PathBuf>,
    /// The environment the next command runs in, whole: the user's login shell's as resolved
    /// when this shell was opened, plus every assignment made in it since.
    pub env: std::collections::HashMap<String, String>,
    /// The assignments made in this shell, in order, one per name — the part of `env` that is
    /// the user's own. What a restart replays over a freshly resolved environment.
    pub assignments: Vec<(String, String)>,
    /// Bumped by every change to this shell's state — a run pushed, output arrived, a status
    /// set, the directory moved. What the backup flush compares against, since most of that
    /// state is not in the transcript's text.
    pub generation: u64,
    /// The generation, and the input's revision, the on-disk snapshot last captured.
    pub backed_up: Option<(u64, aether_protocol::Revision)>,
    /// `Shell N` — what the picker row, the status bar and the outline's group call this shell.
    /// Held here as well as on the buffer's `VirtualSource` so the outline can name it without
    /// walking back out to the document.
    pub title: String,
    /// Source of run ids, unique within this shell.
    next_run: RunId,
}

impl Transcript {
    pub fn new(input: BufferId, cwd: PathBuf, title: String) -> Self {
        Transcript {
            runs: Vec::new(),
            input,
            cwd,
            prev_cwd: None,
            env: std::collections::HashMap::new(),
            assignments: Vec::new(),
            generation: 0,
            backed_up: None,
            title,
            next_run: 1,
        }
    }

    /// The run in flight, if any. **One at a time per shell**, so this is a `find` rather than a
    /// filter: a second `Enter` while it is `Some` is refused.
    pub fn active(&self) -> Option<&Run> {
        self.runs.iter().find(|r| r.status == RunStatus::Running)
    }

    pub fn active_mut(&mut self) -> Option<&mut Run> {
        self.runs
            .iter_mut()
            .find(|r| r.status == RunStatus::Running)
    }

    pub fn run(&self, id: RunId) -> Option<&Run> {
        self.runs.iter().find(|r| r.id == id)
    }

    pub fn run_mut(&mut self, id: RunId) -> Option<&mut Run> {
        self.runs.iter_mut().find(|r| r.id == id)
    }

    /// Append a run starting at `start_line` of the transcript, in the shell's current directory,
    /// and answer its id.
    pub fn push_run(&mut self, command: String, start_line: u32, cancel: CancelHandle) -> RunId {
        let id = self.next_run;
        self.next_run += 1;
        self.runs.push(Run {
            id,
            command,
            cwd: self.cwd.clone(),
            start_line,
            end_line_exclusive: start_line,
            status: RunStatus::Running,
            elapsed_ms: None,
            cancel: Some(cancel),
        });
        id
    }

    /// Set a variable for the runs that follow, remembering it as the user's own.
    pub fn assign(&mut self, name: String, value: String) {
        self.env.insert(name.clone(), value.clone());
        match self.assignments.iter_mut().find(|(n, _)| *n == name) {
            Some(slot) => slot.1 = value,
            None => self.assignments.push((name, value)),
        }
    }

    /// Everything a restart needs to bring this shell back: the transcript text and the runs over
    /// it, where it is, what it has assigned, and what was being typed. A run still going is
    /// recorded as killed — the process will not survive the server that started it.
    pub fn snapshot(&self, text: &str, input: &str) -> ShellSnapshot {
        ShellSnapshot {
            version: 1,
            cwd: self.cwd.clone(),
            prev_cwd: self.prev_cwd.clone(),
            assignments: self.assignments.clone(),
            input: input.to_string(),
            text: text.to_string(),
            runs: self
                .runs
                .iter()
                .map(|r| RunSnapshot {
                    command: r.command.clone(),
                    cwd: r.cwd.clone(),
                    start_line: r.start_line,
                    end_line_exclusive: r.end_line_exclusive,
                    status: match r.status {
                        RunStatus::Running => RunStatus::Killed,
                        other => other,
                    },
                    elapsed_ms: r.elapsed_ms,
                })
                .collect(),
        }
    }

    /// A shell as a snapshot left it, over `base_env` — the environment resolved afresh for the
    /// directory, with the snapshot's assignments replayed on top. Answers the transcript text
    /// to seed the document with.
    pub fn from_snapshot(
        input: BufferId,
        title: String,
        snap: ShellSnapshot,
        base_env: std::collections::HashMap<String, String>,
    ) -> (Self, String) {
        let mut t = Transcript::new(input, snap.cwd, title);
        t.prev_cwd = snap.prev_cwd;
        t.env = base_env;
        for (name, value) in snap.assignments {
            t.assign(name, value);
        }
        for r in snap.runs {
            let id = t.next_run;
            t.next_run += 1;
            t.runs.push(Run {
                id,
                command: r.command,
                cwd: r.cwd,
                start_line: r.start_line,
                end_line_exclusive: r.end_line_exclusive,
                status: r.status,
                elapsed_ms: r.elapsed_ms,
                cancel: None,
            });
        }
        (t, snap.text)
    }

    /// Ask every unfinished run's process group to stop — closing the view, and shutting the
    /// server down. A shell whose view is gone has nobody to report to, and a `cargo build` that
    /// outlives the window it was started from is exactly the orphan the process group exists to
    /// prevent.
    pub fn cancel_all(&mut self) {
        for run in &mut self.runs {
            if let Some(handle) = run.cancel.take() {
                let _ = handle.send(true);
            }
        }
    }
}

/// The other end of a run's cancellation — see [`crate::process`].
pub type CancelHandle = crate::process::CancelHandle;

/// A shell written down, for the backup file. See [`Transcript::snapshot`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ShellSnapshot {
    pub version: u32,
    pub cwd: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev_cwd: Option<PathBuf>,
    #[serde(default)]
    pub assignments: Vec<(String, String)>,
    #[serde(default)]
    pub input: String,
    pub text: String,
    pub runs: Vec<RunSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RunSnapshot {
    pub command: String,
    pub cwd: PathBuf,
    pub start_line: u32,
    pub end_line_exclusive: u32,
    pub status: RunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
}

/// Most transcript text a snapshot keeps. A shell that has been running builds all day holds far
/// more than anyone will scroll back through after a restart; the oldest runs go first.
pub const SNAPSHOT_BUDGET: usize = 2 * 1024 * 1024;

impl ShellSnapshot {
    /// This snapshot with the oldest runs dropped until the text fits `budget` bytes. Whole runs,
    /// never part of one, so every kept run's lines still name its own output; line numbers are
    /// rebased to the text that remains.
    pub fn trimmed(mut self, budget: usize) -> Self {
        if self.text.len() <= budget {
            return self;
        }
        // Byte offset of each line start, so a run's first line maps to a cut point.
        let line_starts: Vec<usize> = std::iter::once(0)
            .chain(self.text.match_indices('\n').map(|(i, _)| i + 1))
            .collect();
        // Never the newest run: a shell that comes back with nothing at all is worse than one
        // that comes back over budget.
        let mut drop = 0;
        while drop + 1 < self.runs.len() {
            let first_kept = self.runs[drop].start_line as usize;
            let cut = line_starts
                .get(first_kept)
                .copied()
                .unwrap_or(self.text.len());
            if self.text.len() - cut <= budget {
                break;
            }
            drop += 1;
        }
        if drop == 0 {
            return self;
        }
        let first_line = self
            .runs
            .get(drop)
            .map(|r| r.start_line)
            .unwrap_or(self.runs.last().map_or(0, |r| r.end_line_exclusive));
        let cut = line_starts
            .get(first_line as usize)
            .copied()
            .unwrap_or(self.text.len());
        self.text = self.text[cut..].to_string();
        self.runs.drain(..drop);
        for r in &mut self.runs {
            r.start_line -= first_line;
            r.end_line_exclusive -= first_line;
        }
        self
    }
}

/// One command, and what became of it.
#[derive(Debug)]
pub struct Run {
    pub id: RunId,
    /// As submitted, trimmed. Never re-read from the input: the input has been cleared by now, and
    /// what a box shows is what actually ran.
    pub command: String,
    /// Where it ran — for a directory change, where it arrived. Its own copy, because the shell's
    /// directory moves on and this box must keep saying where this command happened.
    pub cwd: PathBuf,
    /// First line of the transcript this run's output occupies.
    pub start_line: u32,
    /// One past its last line. Grows as output arrives; fixed once the run ends.
    pub end_line_exclusive: u32,
    pub status: RunStatus,
    /// How long it took, once it is over.
    pub elapsed_ms: Option<u64>,
    /// Taken when the run ends or is cancelled, so a second cancel is a no-op rather than a
    /// signal to a pid that has been recycled.
    pub cancel: Option<CancelHandle>,
}

impl Run {
    pub fn is_running(&self) -> bool {
        self.status == RunStatus::Running
    }

    /// What the wire says about this run.
    pub fn state(&self) -> aether_protocol::shell::RunState {
        aether_protocol::shell::RunState {
            run: self.id,
            command: self.command.clone(),
            status: self.status,
        }
    }
}

/// The name on one run's box, drawn on its top border: where it ran and — once it is over — how
/// it went and how long it took.
///
/// A title rather than a row of its own because the input at the foot of the transcript wears the
/// same one with nothing yet to report ([`input_title`]): press `Enter` and the box you were
/// typing in gains an outcome and a duration, the line you typed becomes the row under its title,
/// and the output follows. The command moves up into the history in the shape it already had,
/// rather than being re-set into a different one.
///
/// Styled with a patch's own roles, deliberately: a shell is another composed view, and inventing
/// a palette for it would add roles to the one part of the theme with no cross-shell parity test.
pub fn run_title(run: &Run) -> Vec<Element> {
    box_title(&run.cwd, Some(run))
}

/// The name on the input's box: the directory alone — the box a run's title will finish the
/// moment this input is submitted. See [`run_title`].
pub fn input_title(cwd: &Path) -> Vec<Element> {
    box_title(cwd, None)
}

/// The chrome inside one run's box: what ran, bare, on the row under the box's title. The input's
/// box has none — the line you type into is the only thing in it.
pub fn command_row(run: &Run) -> Vec<Element> {
    use crate::patch::FILE;
    // A multi-line command (`Alt-Enter`) still has to fit one row, and a visible break marker
    // reads better than a silently truncated first line.
    let command = run.command.replace('\n', " ⏎ ");
    let highlights = vec![highlight(0, command.len(), FILE)];
    vec![Element::chrome(vec![Element::row(vec![Element::text(
        command, highlights,
    )])])]
}

/// The inline nodes a box's title is made of: where it ran and — for a finished run — how it went
/// and how long it took. One builder for both shapes, so the input's title and a run's can never
/// drift apart in anything but the outcome.
///
/// No rule and no padding of its own: what surrounds a title on the border row is the border's
/// business, and each shell draws that in its own alphabet.
fn box_title(cwd: &Path, run: Option<&Run>) -> Vec<Element> {
    use crate::patch::{ADDED, META, REMOVED};

    let mut text = String::new();
    let mut highlights = Vec::new();
    let mut push = |s: &str, role: &'static str| {
        let start = text.len();
        text.push_str(s);
        highlights.push(highlight(start, text.len(), role));
    };

    push(&display_path(cwd), META);
    if let Some(run) = run {
        match run.status {
            RunStatus::Running => {
                push("  ", META);
                push("running · Space Alt-b stops it", META);
            }
            status => {
                let role = if status == (RunStatus::Exited { code: 0 }) {
                    ADDED
                } else {
                    REMOVED
                };
                push("  ", META);
                push(&status.label(), role);
                if let Some(ms) = run.elapsed_ms {
                    push("  ", META);
                    push(&format_elapsed(ms), META);
                }
            }
        }
    }

    vec![Element::text(text, highlights)]
}

fn highlight(start: usize, end: usize, kind: &str) -> aether_protocol::viewport::Highlight {
    aether_protocol::viewport::Highlight {
        start: start as u32,
        end: end as u32,
        kind: kind.to_string(),
    }
}

/// `$HOME` shortened to `~`, as every shell prompt does — a header naming an absolute path three
/// levels deep spends the whole row on saying where you already are.
pub(crate) fn display_path(path: &Path) -> String {
    let full = path.to_string_lossy().into_owned();
    let Some(home) = std::env::var_os("HOME") else {
        return full;
    };
    let home = home.to_string_lossy();
    if home.is_empty() {
        return full;
    }
    match full.strip_prefix(home.as_ref()) {
        Some("") => "~".into(),
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => full,
    }
}

/// A duration in the units a person reads at a glance: sub-second in milliseconds, then seconds to
/// one decimal, then minutes and seconds.
fn format_elapsed(ms: u64) -> String {
    if ms < 1000 {
        return format!("{ms}ms");
    }
    let secs = ms as f64 / 1000.0;
    if secs < 60.0 {
        return format!("{secs:.1}s");
    }
    let total = ms / 1000;
    format!("{}m{:02}s", total / 60, total % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(command: &str, status: RunStatus, elapsed_ms: Option<u64>) -> Run {
        Run {
            id: 1,
            command: command.into(),
            cwd: PathBuf::from("/tmp/project"),
            start_line: 0,
            end_line_exclusive: 1,
            status,
            elapsed_ms,
            cancel: None,
        }
    }

    /// The text of a title, left to right.
    fn title_text(nodes: &[Element]) -> String {
        nodes.iter().map(Element::text_content).collect()
    }

    /// A run's box says where it ran and — once it is over — how it went and how long it took, on
    /// its border; and what ran, bare, on the row inside it.
    #[test]
    fn a_runs_box_names_the_directory_and_the_outcome_and_holds_the_command() {
        let running = run("cargo build", RunStatus::Running, None);
        let title = title_text(&run_title(&running));
        assert!(title.starts_with("/tmp/project"), "{title:?}");
        assert!(
            title.contains("Space Alt-b"),
            "a running command says how to stop it: {title:?}"
        );
        let rows: Vec<String> = command_row(&running)
            .iter()
            .map(Element::text_content)
            .collect();
        assert_eq!(rows, vec!["cargo build".to_string()], "the command, bare");

        let done = run("cargo build", RunStatus::Exited { code: 101 }, Some(1500));
        let title = title_text(&run_title(&done));
        assert!(title.contains("exit 101"), "{title}");
        assert!(title.contains("1.5s"), "{title}");
        assert!(!title.contains("Space Alt-b"), "nothing to stop: {title}");
    }

    /// The roles are the patch's, not new ones: a failing run reads in the same red a removed
    /// line does, and a clean one in the same green.
    #[test]
    fn a_titles_outcome_takes_the_patchs_own_roles() {
        let roles = |status| {
            run_title(&run("x", status, Some(10)))
                .iter()
                .flat_map(|e| e.highlight_runs())
                .map(|h| h.kind.clone())
                .collect::<Vec<_>>()
        };
        assert!(roles(RunStatus::Exited { code: 0 }).contains(&crate::patch::ADDED.to_string()));
        assert!(roles(RunStatus::Exited { code: 1 }).contains(&crate::patch::REMOVED.to_string()));
        assert!(roles(RunStatus::Killed).contains(&crate::patch::REMOVED.to_string()));
        // And the command itself is always the file role — the heaviest thing in the box.
        let command_roles: Vec<String> = command_row(&run("x", RunStatus::Killed, None))
            .iter()
            .flat_map(|e| e.highlight_runs())
            .map(|h| h.kind.clone())
            .collect();
        assert!(command_roles.contains(&crate::patch::FILE.to_string()));
    }

    /// A multi-line command still occupies one row.
    #[test]
    fn a_multiline_command_stays_one_row() {
        let rows = command_row(&run("echo one\necho two", RunStatus::Running, None));
        let text: String = rows.iter().map(Element::text_content).collect();
        assert!(!text.contains('\n'), "{text}");
        assert!(text.contains("echo one ⏎ echo two"), "{text}");
    }

    /// The input's box wears a run's title with nothing yet to report: the same directory in the
    /// same place, so submitting only ever *adds* to the box you were typing in.
    #[test]
    fn the_input_wears_a_runs_title_with_nothing_to_report() {
        let cwd = Path::new("/tmp/project");
        let input = input_title(cwd);
        assert_eq!(title_text(&input), "/tmp/project");

        let running = run("cargo build", RunStatus::Running, None);
        let started = title_text(&run_title(&running));
        assert!(
            started.starts_with(&title_text(&input)),
            "{started:?} does not begin as the input's title does"
        );
    }

    /// A snapshot carries everything a restart needs, and a run still going comes back killed —
    /// the process does not survive the server.
    #[test]
    fn a_snapshot_round_trips_and_kills_what_was_running() {
        let (handle, _token) = crate::process::cancel_channel();
        let mut t = Transcript::new(9, PathBuf::from("/tmp/p"), "Shell 1".into());
        t.prev_cwd = Some(PathBuf::from("/tmp"));
        t.assign("FOO".into(), "bar".into());
        t.assign("FOO".into(), "baz".into());
        let a = t.push_run("echo one".into(), 0, handle);
        t.run_mut(a).unwrap().end_line_exclusive = 1;
        t.run_mut(a).unwrap().status = RunStatus::Exited { code: 0 };
        let (handle, _token) = crate::process::cancel_channel();
        t.push_run("sleep 100".into(), 1, handle);

        let snap = t.snapshot("one\n", "typed ahead");
        let json = serde_json::to_string(&snap).unwrap();
        let back: ShellSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, snap);
        assert_eq!(back.runs[1].status, RunStatus::Killed);
        assert_eq!(
            back.assignments,
            vec![("FOO".to_string(), "baz".to_string())]
        );

        let mut base = std::collections::HashMap::new();
        base.insert("PATH".to_string(), "/usr/bin".to_string());
        let (restored, text) = Transcript::from_snapshot(11, "Shell 1".into(), back, base);
        assert_eq!(text, "one\n");
        assert_eq!(restored.cwd, PathBuf::from("/tmp/p"));
        assert_eq!(
            restored.prev_cwd.as_deref(),
            Some(std::path::Path::new("/tmp"))
        );
        assert_eq!(restored.env.get("FOO").map(String::as_str), Some("baz"));
        assert_eq!(
            restored.env.get("PATH").map(String::as_str),
            Some("/usr/bin")
        );
        assert_eq!(restored.runs.len(), 2);
        assert!(
            restored.active().is_none(),
            "nothing is running after a restart"
        );
        assert_eq!(restored.runs[1].command, "sleep 100");
        let (handle, _token) = crate::process::cancel_channel();
        let next = restored.runs.len() as u64 + 1;
        let mut restored = restored;
        assert_eq!(
            restored.push_run("x".into(), 1, handle),
            next,
            "ids continue"
        );
    }

    /// Trimming drops whole runs from the front until the text fits, and rebases what is kept.
    #[test]
    fn trimming_drops_the_oldest_runs_whole() {
        let run = |command: &str, start: u32, end: u32| RunSnapshot {
            command: command.into(),
            cwd: PathBuf::from("/p"),
            start_line: start,
            end_line_exclusive: end,
            status: RunStatus::Exited { code: 0 },
            elapsed_ms: None,
        };
        let snap = ShellSnapshot {
            version: 1,
            cwd: PathBuf::from("/p"),
            prev_cwd: None,
            assignments: Vec::new(),
            input: String::new(),
            text: "aaaa\nbb\nc\n".into(),
            runs: vec![run("a", 0, 1), run("b", 1, 2), run("c", 2, 3)],
        };
        let kept = snap.clone().trimmed(100);
        assert_eq!(kept, snap, "under budget, untouched");
        let kept = snap.clone().trimmed(5);
        assert_eq!(kept.text, "bb\nc\n");
        assert_eq!(kept.runs.len(), 2);
        assert_eq!(
            (kept.runs[0].start_line, kept.runs[0].end_line_exclusive),
            (0, 1)
        );
        assert_eq!(
            (kept.runs[1].start_line, kept.runs[1].end_line_exclusive),
            (1, 2)
        );
        let kept = snap.trimmed(1);
        assert_eq!(
            kept.text, "c\n",
            "even one run over budget keeps the newest whole"
        );
        assert_eq!(kept.runs.len(), 1);
    }

    #[test]
    fn elapsed_reads_at_a_glance() {
        assert_eq!(format_elapsed(0), "0ms");
        assert_eq!(format_elapsed(999), "999ms");
        assert_eq!(format_elapsed(1000), "1.0s");
        assert_eq!(format_elapsed(59_940), "59.9s");
        assert_eq!(format_elapsed(60_000), "1m00s");
        assert_eq!(format_elapsed(3_725_000), "62m05s");
    }

    /// One run at a time: the active run is the one still going, and it stops being active the
    /// moment it has a status.
    #[test]
    fn only_one_run_is_ever_active() {
        let (handle, _token) = crate::process::cancel_channel();
        let mut t = Transcript::new(9, PathBuf::from("/tmp"), "Shell 1".into());
        assert!(t.active().is_none());
        let first = t.push_run("a".into(), 0, handle);
        assert_eq!(t.active().map(|r| r.id), Some(first));
        t.run_mut(first).unwrap().status = RunStatus::Exited { code: 0 };
        assert!(t.active().is_none());
        let (handle, _token) = crate::process::cancel_channel();
        let second = t.push_run("b".into(), 1, handle);
        assert_ne!(first, second, "run ids are not reused");
        assert_eq!(t.active().map(|r| r.id), Some(second));
    }

    /// Closing a shell stops everything it started, and says so exactly once.
    #[test]
    fn cancel_all_signals_every_unfinished_run() {
        let (handle, mut token) = crate::process::cancel_channel();
        let mut t = Transcript::new(9, PathBuf::from("/tmp"), "Shell 1".into());
        t.push_run("sleep 100".into(), 0, handle);
        assert!(!*token.borrow_and_update());
        t.cancel_all();
        assert!(*token.borrow_and_update(), "the run was told to stop");
        // Idempotent: the handle is taken, so a second pass has nothing to send.
        t.cancel_all();
        assert!(t.runs[0].cancel.is_none());
    }

    #[test]
    fn home_is_shortened_the_way_a_prompt_shortens_it() {
        // Set for this test only; the function reads it per call.
        let home = std::env::var_os("HOME").map(|h| h.to_string_lossy().into_owned());
        let Some(home) = home.filter(|h| h.starts_with('/')) else {
            return; // no usable HOME in this environment
        };
        assert_eq!(display_path(Path::new(&home)), "~");
        assert_eq!(display_path(&Path::new(&home).join("src")), "~/src");
        assert_eq!(display_path(Path::new("/etc")), "/etc");
        // A sibling directory whose name merely starts with the home path is not under it.
        assert_eq!(
            display_path(Path::new(&format!("{home}x"))),
            format!("{home}x")
        );
    }
}

// ---- following a line ----------------------------------------------------------------------------

/// A `path[:line[:col]]` a tool printed, as found in one line of output.
///
/// Lines are 1-based on the wire of every tool that prints them and 0-based everywhere in this
/// editor; the conversion happens here, once, so nothing downstream has to remember which it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub path: String,
    /// 0-based.
    pub line: u32,
    /// 0-based.
    pub col: u32,
}

/// The file location a line of shell output points at, if it points at one.
///
/// Four shapes, which between them cover what actually gets printed into a shell:
///
/// - `src/main.rs:12:5` — rustc, cargo's `-->`, gcc, eslint, ripgrep, `grep -n` (which stops at the
///   line and has no column);
/// - `src/app.ts(12,5)` — tsc and the MSVC-flavoured tools;
/// - `File "x.py", line 12` — a Python traceback;
/// - a bare `src/main.rs` with neither, which lands at the top of the file.
///
/// Deliberately **not** a general grammar. The cost of a wrong answer is opening a file you didn't
/// ask for, and the guard against it is not a cleverer regex but the caller's existence check: a
/// timestamp like `10:30:45` parses here as a path called `10`, and there is no such file.
pub fn parse_location(text: &str) -> Option<Location> {
    // Lead-ins tools put before the path: cargo's arrow, a stack frame's `at`, indentation.
    let text = text.trim();
    let text = text
        .strip_prefix("-->")
        .or_else(|| text.strip_prefix("at "))
        .unwrap_or(text)
        .trim();

    // `File "path", line N` — a Python traceback, which quotes the path and so is unambiguous.
    if let Some(rest) = text.strip_prefix("File \"") {
        let (path, rest) = rest.split_once('"')?;
        let line = rest
            .split_once("line ")
            .and_then(|(_, n)| take_number(n))
            .unwrap_or(1);
        return Some(Location {
            path: path.to_string(),
            line: line.saturating_sub(1),
            col: 0,
        });
    }

    // `path(line,col)` — tsc. Checked before the colon form because the path itself may contain
    // colons on no platform we support, but its *suffix* here is parenthesised rather than colonised.
    if let Some((path, rest)) = text.split_once('(') {
        if let Some((inside, _)) = rest.split_once(')') {
            if let Some((l, c)) = inside.split_once(',') {
                if let (Some(line), Some(col)) = (take_number(l), take_number(c.trim())) {
                    if !path.is_empty() {
                        return Some(Location {
                            path: path.trim().to_string(),
                            line: line.saturating_sub(1),
                            col: col.saturating_sub(1),
                        });
                    }
                }
            }
        }
    }

    // `path:line[:col]`, and a bare path. Split from the left on the first colon that is followed
    // by a digit: a path may contain colons, and a line number may not.
    let mut parts = text.split(':');
    let path = parts.next()?.trim();
    if path.is_empty() {
        return None;
    }
    let line = parts.next().and_then(take_number);
    let col = line.and(parts.next()).and_then(take_number);
    Some(Location {
        path: path.to_string(),
        line: line.unwrap_or(1).saturating_sub(1),
        col: col.unwrap_or(1).saturating_sub(1),
    })
}

/// The leading run of digits of `s`, as a number — `12` from `12:5` and from `12, in <module>`.
fn take_number(s: &str) -> Option<u32> {
    let digits: String = s
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// The absolute path a [`Location`] names, resolved against the run's working directory, or `None`
/// when there is no such file.
///
/// The existence check is what makes a permissive parse safe: everything that looks like a path
/// but isn't one — a timestamp, a ratio, a URL's port — resolves to nothing and `Enter` stays the
/// quiet no-op it is on any other line.
pub fn resolve(location: &Location, cwd: &Path) -> Option<PathBuf> {
    let candidate = if Path::new(&location.path).is_absolute() {
        PathBuf::from(&location.path)
    } else {
        cwd.join(&location.path)
    };
    candidate.is_file().then_some(candidate)
}

#[cfg(test)]
mod follow_tests {
    use super::*;

    fn at(text: &str) -> Option<(String, u32, u32)> {
        parse_location(text).map(|l| (l.path, l.line, l.col))
    }

    /// The shapes tools actually print, each converted to this editor's 0-based coordinates.
    #[test]
    fn the_common_compiler_shapes_parse() {
        // rustc / cargo — the arrow is a lead-in, not part of the path.
        assert_eq!(
            at("  --> src/main.rs:12:5"),
            Some(("src/main.rs".into(), 11, 4))
        );
        assert_eq!(
            at("src/lib.rs:3:1: error: expected `;`"),
            Some(("src/lib.rs".into(), 2, 0))
        );
        // ripgrep / `grep -n`: a line and no column.
        assert_eq!(
            at("src/a.rs:40:    let x = 1;"),
            Some(("src/a.rs".into(), 39, 0))
        );
        // tsc / MSVC.
        assert_eq!(
            at("src/app.ts(12,5): error TS2322"),
            Some(("src/app.ts".into(), 11, 4))
        );
        // A Python traceback quotes its path, which is what makes a path with spaces work.
        assert_eq!(
            at("  File \"/tmp/my dir/x.py\", line 7, in <module>"),
            Some(("/tmp/my dir/x.py".into(), 6, 0))
        );
        // A stack frame's lead-in.
        assert_eq!(at("    at src/x.js:9:3"), Some(("src/x.js".into(), 8, 2)));
        // A bare path lands at the top.
        assert_eq!(at("Cargo.toml"), Some(("Cargo.toml".into(), 0, 0)));
    }

    /// A permissive parse is safe because the caller checks the file exists: these all parse into
    /// something, and none of them resolves.
    #[test]
    fn what_looks_like_a_path_but_is_not_resolves_to_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let nothing = |text: &str| parse_location(text).and_then(|l| resolve(&l, dir.path()));
        assert_eq!(nothing("10:30:45  build finished"), None, "a timestamp");
        assert_eq!(nothing("passed: 42/42"), None, "a ratio");
        assert_eq!(nothing("   "), None, "blank output");
        assert_eq!(parse_location(""), None);
    }

    /// Resolution is relative to the run's own directory, and an absolute path is taken as it is.
    #[test]
    fn a_relative_path_resolves_against_the_runs_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();

        let located = parse_location("  --> src/main.rs:2:3").unwrap();
        assert_eq!(resolve(&located, &root), Some(root.join("src/main.rs")));
        // Same file named absolutely.
        let abs = format!("{}:2:3", root.join("src/main.rs").display());
        let located = parse_location(&abs).unwrap();
        assert_eq!(
            resolve(&located, Path::new("/nowhere")),
            Some(root.join("src/main.rs"))
        );
        // A directory is not somewhere `Enter` can land.
        let located = parse_location("src").unwrap();
        assert_eq!(resolve(&located, &root), None);
    }
}
