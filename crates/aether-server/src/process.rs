//! Running a child process and reading what it says, as it says it.
//!
//! Two callers with one problem: [`crate::git_cli::run_streaming`] wants git's progress while a
//! push sits in a TCP timeout, and the shell view wants a command's output while it is still
//! producing it. Both want the child *killable*, and both want the kill to land on everything the
//! child started rather than only on the child — a `git push` spawns `ssh`, and `sh -c "cargo
//! build"` spawns a compiler that will happily keep going after its shell is gone.
//!
//! So a child gets a **process group of its own** ([`command`]) and a cancel kills the *group*
//! ([`kill_group`]). That is the whole reason this module exists rather than each caller doing its
//! own `select!` over two pipes: the second copy is the one that forgets the group.
//!
//! The other half is [`OutputText`] — turning a stream of arbitrary bytes into the text a document
//! can hold: UTF-8 decoded across chunk boundaries, ANSI escapes stripped, and a carriage return
//! rewriting the line it lands in rather than adding one. Kept here beside the reading because it
//! is a fact about process output, and it is the part with all the edge cases.
//!
//! And one thing that is *not* about any single child: [`shed_build_environment`], which decides
//! what every child does **not** inherit. It lives here because this is the module that knows what
//! spawning a process means.

use std::ffi::OsStr;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

/// Variables `cargo run` and `cargo test` inject into the process they launch, naming the *build*
/// of that binary. See [`shed_build_environment`] for why they must not outlive it.
const BUILD_CONTEXT_VARS: &[&str] = &[
    "CARGO",
    "CARGO_MANIFEST_DIR",
    "CARGO_MANIFEST_PATH",
    "CARGO_CRATE_NAME",
    "CARGO_BIN_NAME",
    "CARGO_PRIMARY_PACKAGE",
    "CARGO_TARGET_TMPDIR",
    "OUT_DIR",
    // rustup's re-entry guard, injected by its cargo/rustc proxies. Not a fingerprint input, but
    // inherited it counts a depth the child never descended, and enough layers of it make rustup
    // refuse to run at all with "infinite recursion detected".
    "RUST_RECURSION_COUNT",
];

/// The injected *families*: `CARGO_PKG_*` (eleven of them, the crate's own manifest metadata) and,
/// under `cargo test`, one `CARGO_BIN_EXE_<name>` per binary in the package.
const BUILD_CONTEXT_PREFIXES: &[&str] = &["CARGO_PKG_", "CARGO_BIN_EXE_"];

/// Whether `key` names build context rather than something the user's environment set.
///
/// The distinction matters: `CARGO_HOME`, `RUSTUP_HOME` and `RUSTUP_TOOLCHAIN` look like they
/// belong to this list and do not. The first two are ordinary user configuration that a child
/// running `cargo` still needs, and the third is how the toolchain that built us tells its own
/// children which toolchain to be — dropping it would silently move every `cargo` a shell view
/// runs onto whatever `rust-toolchain.toml` pins, which is a different build, in a different set
/// of units, on a disk that does not have room for a third.
fn is_build_context(key: &str) -> bool {
    BUILD_CONTEXT_VARS.contains(&key) || BUILD_CONTEXT_PREFIXES.iter().any(|p| key.starts_with(p))
}

/// Drop the build context `cargo run` left in this process, so that nothing spawned from here can
/// inherit it. Call once, first thing in `main`.
///
/// In development the daemon is started by `cargo run`, and cargo hands the binary it launches a
/// description of the build it just did: `OUT_DIR`, `CARGO_MANIFEST_DIR`, `CARGO_PKG_*`. Those
/// describe `ae`. They are meaningless to a language server, a shell command or an agent — and
/// they are not inert.
///
/// `ring`'s build script declares `cargo:rerun-if-env-changed` on all six, and cargo tests such a
/// variable against the environment of *the cargo process that is asking*. So a `cargo check` run
/// by the rust-analyzer we spawned sees them set; a `cargo build` the user runs in their own
/// terminal sees them unset; and ring's build script re-runs on every alternation, taking `rustls`,
/// `rustls-webpki`, `ureq` and both leaf crates with it — about ten seconds, on a loop, for as long
/// as the editor is open on its own source.
///
/// Shedding it here rather than at each spawn is the point. There are five places in this crate
/// that start a child and a sixth that asks the user's login shell to describe its environment
/// ([`crate::lsp::shell_env`]) — and that last one *inherits*, so a scrub applied per-seam would be
/// undone by the very map the other seams overlay. One process-wide removal, before there is a
/// second thread to race, leaves nothing for a new seam to forget.
///
/// # Safety and scope
///
/// `remove_var` is only sound while the process is single-threaded, which is why this must run
/// before the runtime is built (it becomes `unsafe` to call at all in edition 2024). That also
/// means it is deliberately *not* called by the server itself: a test server shares its process
/// with the test harness's threads, so an in-process server still inherits whatever `cargo test`
/// injected. Tests spawn dummy language servers and commands in temporary directories, so nothing
/// there reaches a build fingerprint.
pub fn shed_build_environment() {
    let doomed: Vec<String> = std::env::vars_os()
        .filter_map(|(k, _)| k.into_string().ok())
        .filter(|k| is_build_context(k))
        .collect();
    for key in doomed {
        std::env::remove_var(key);
    }
}

/// How long a cancelled group gets to exit on `SIGTERM` before `SIGKILL`. Long enough for a
/// shell to run a trap and for git to unlink a lock file; short enough that `Space v c` feels
/// like it worked.
const KILL_GRACE: Duration = Duration::from_millis(300);

/// Which pipe a chunk of output arrived on.
///
/// Reported rather than merged so `git_cli` can keep its two strings apart (its callers read
/// git's stderr verbatim) while the shell view interleaves them — the arrival *order* is the same
/// either way, and it is the order the reader saw things happen in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// What a run produced: the exit code, or `None` when the child was killed by a signal — which is
/// exactly what a cancel leaves, and why callers read cancellation off their own token rather than
/// out of the status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exit {
    pub code: Option<i32>,
}

/// Watches for a cancellation of a running child.
///
/// A `watch` channel rather than a bare notification: it carries *state*, so a cancel that lands
/// before the runner gets around to waiting is still seen. A dropped sender means nothing can
/// cancel this any more, which reads as "never", not "now".
pub type CancelToken = tokio::sync::watch::Receiver<bool>;
/// The other end of a [`CancelToken`] — held by whoever may cancel the run.
pub type CancelHandle = tokio::sync::watch::Sender<bool>;

pub fn cancel_channel() -> (CancelHandle, CancelToken) {
    tokio::sync::watch::channel(false)
}

async fn cancelled(rx: &mut CancelToken) {
    loop {
        if *rx.borrow_and_update() {
            return;
        }
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// A child prepared the way both runners want it: nothing on stdin, both pipes captured, killed
/// if the future holding it is dropped, and — on unix — in a **process group of its own**.
///
/// The group is the load-bearing part. Without it a cancel kills the direct child and leaves
/// whatever it started running with its pipes still open, so the reader never sees EOF and the
/// work the user asked to stop carries on.
pub fn command(program: impl AsRef<OsStr>) -> Command {
    let mut cmd = Command::new(program);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    cmd
}

/// Signal every process in the group led by `pid` — the group [`command`] put the child in.
///
/// A no-op off unix, and a no-op for a group that has already gone: `killpg` failing means there
/// is nothing left to kill, which is the outcome we wanted.
pub fn kill_group(pid: u32, signal: i32) {
    #[cfg(unix)]
    // SAFETY: `killpg` takes a pid and a signal number and returns an error rather than
    // misbehaving for a group that no longer exists. Nothing here is dereferenced.
    unsafe {
        libc::killpg(pid as libc::pid_t, signal);
    }
    #[cfg(not(unix))]
    let _ = (pid, signal);
}

/// Kill the group led by `pid`: `SIGTERM`, then `SIGKILL` after [`KILL_GRACE`] if it is still
/// there. Returns once the direct child has been reaped.
async fn terminate(child: &mut tokio::process::Child) -> std::io::Result<std::process::ExitStatus> {
    let Some(pid) = child.id() else {
        // Already reaped; `wait` answers from the cached status.
        return child.wait().await;
    };
    kill_group(pid, libc_sigterm());
    match tokio::time::timeout(KILL_GRACE, child.wait()).await {
        Ok(status) => status,
        Err(_) => {
            kill_group(pid, libc_sigkill());
            child.wait().await
        }
    }
}

#[cfg(unix)]
pub(crate) fn libc_sigterm() -> i32 {
    libc::SIGTERM
}
#[cfg(unix)]
fn libc_sigkill() -> i32 {
    libc::SIGKILL
}
#[cfg(not(unix))]
pub(crate) fn libc_sigterm() -> i32 {
    0
}
#[cfg(not(unix))]
fn libc_sigkill() -> i32 {
    0
}

/// Run `cmd` to completion, handing every chunk of output to `on_chunk` **in arrival order**, and
/// killing the child's whole process group if `cancel` fires.
///
/// `cmd` is expected to come from [`command`] — it is taken already configured so a caller can add
/// its own environment, arguments and working directory without this growing a parameter per
/// caller.
///
/// `on_chunk` receives raw bytes, not lines: a progress counter rewriting itself with carriage
/// returns has no lines, and deciding what a line is belongs to whoever is going to display it
/// (see [`OutputText`]). It may return `false` to stop the run — the cap a shell view puts on one
/// command's output, expressed where the bytes are counted rather than as a rule a caller has to
/// remember.
pub async fn run_streaming(
    mut cmd: Command,
    mut cancel: CancelToken,
    mut on_chunk: impl FnMut(Stream, &[u8]) -> bool + Send,
) -> std::io::Result<Exit> {
    let mut child = cmd.spawn()?;
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    // One buffer per stream: the two read branches below are alive in the same `select!`, so they
    // can't share.
    let mut out_buf = [0u8; 8192];
    let mut err_buf = [0u8; 8192];

    let status = loop {
        tokio::select! {
            // Biased so a cancel is honoured even when the child is producing output steadily.
            biased;
            _ = cancelled(&mut cancel) => break terminate(&mut child).await?,
            read = read_some(stderr_pipe.as_mut(), &mut err_buf) => {
                match read? {
                    0 => stderr_pipe = None,
                    n => if !on_chunk(Stream::Stderr, &err_buf[..n]) {
                        break terminate(&mut child).await?;
                    },
                }
            }
            read = read_some(stdout_pipe.as_mut(), &mut out_buf) => {
                match read? {
                    0 => stdout_pipe = None,
                    n => if !on_chunk(Stream::Stdout, &out_buf[..n]) {
                        break terminate(&mut child).await?;
                    },
                }
            }
            status = child.wait(), if stdout_pipe.is_none() && stderr_pipe.is_none() => {
                break status?;
            }
        }
    };
    Ok(Exit {
        code: status.code(),
    })
}

/// Read from a pipe, or park forever when it's already closed — so the `select!` above can drop
/// each stream as it ends without the closed branch spinning at 100% on a perpetual `Ok(0)`.
async fn read_some<R: tokio::io::AsyncRead + Unpin>(
    pipe: Option<&mut R>,
    buf: &mut [u8],
) -> std::io::Result<usize> {
    use tokio::io::AsyncReadExt;
    match pipe {
        Some(r) => r.read(buf).await,
        None => std::future::pending().await,
    }
}

// ---- pipelines ----------------------------------------------------------------------------------

/// One stage of a pipeline, ready to spawn: the program, its arguments, the environment laid over
/// the shell's, and where its two streams come from and go to.
#[derive(Debug, Clone)]
pub struct StageSpec {
    pub program: std::path::PathBuf,
    /// Arguments after the program.
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub stdin: StageStdin,
    pub stdout: StageStdout,
}

#[derive(Debug, Clone)]
pub enum StageStdin {
    Null,
    /// The previous stage's stdout.
    Pipe,
    File(std::path::PathBuf),
}

#[derive(Debug, Clone)]
pub enum StageStdout {
    /// Read by the caller, chunk by chunk, as stderr always is.
    Sink,
    /// The next stage's stdin.
    Pipe,
    File {
        path: std::path::PathBuf,
        append: bool,
    },
}

/// Run a pipeline to completion: every stage spawned in **one process group**, each stage's stdout
/// wired to the next's stdin, every stage's stderr and the last stage's stdout handed to `on_chunk`
/// in arrival order. Answers each stage's exit code, `None` for one killed by a signal.
///
/// Redirection targets are opened first, so a file that cannot be opened is an error before
/// anything has started, and a program that cannot be spawned kills the stages already running.
/// `cancel` firing, or `on_chunk` answering `false`, kills the group.
pub async fn run_pipeline(
    stages: Vec<StageSpec>,
    cwd: &std::path::Path,
    env: &std::collections::HashMap<String, String>,
    mut cancel: CancelToken,
    mut on_chunk: impl FnMut(Stream, &[u8]) -> bool + Send,
) -> std::io::Result<Vec<Option<i32>>> {
    use std::fs::{File, OpenOptions};
    use tokio::sync::mpsc;

    fn at(e: std::io::Error, path: &std::path::Path) -> std::io::Error {
        std::io::Error::new(e.kind(), format!("{}: {e}", path.display()))
    }

    let mut children: Vec<tokio::process::Child> = Vec::with_capacity(stages.len());
    let (tx, mut rx) = mpsc::unbounded_channel::<(Stream, Vec<u8>)>();
    let mut carry: Option<tokio::process::ChildStdout> = None;
    let mut leader: Option<u32> = None;
    for stage in stages {
        let mut cmd = Command::new(&stage.program);
        cmd.args(&stage.args)
            .current_dir(cwd)
            .env_clear()
            .envs(env)
            .envs(stage.env.iter().cloned())
            // Not a terminal, and saying so: a tool told plainly produces readable output instead
            // of cursor choreography that would then be thrown away.
            .env("TERM", "dumb")
            .env("NO_COLOR", "1")
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(leader.map_or(0, |pid| pid as i32));
        match stage.stdin {
            StageStdin::Null => {
                cmd.stdin(Stdio::null());
            }
            StageStdin::Pipe => match carry.take() {
                Some(prev) => {
                    let fd = prev.into_owned_fd().map_err(|e| at(e, &stage.program))?;
                    cmd.stdin(Stdio::from(fd));
                }
                None => {
                    cmd.stdin(Stdio::null());
                }
            },
            StageStdin::File(path) => {
                let file = File::open(&path).map_err(|e| at(e, &path))?;
                cmd.stdin(Stdio::from(file));
            }
        }
        let piped_out = match &stage.stdout {
            StageStdout::Sink | StageStdout::Pipe => {
                cmd.stdout(Stdio::piped());
                true
            }
            StageStdout::File { path, append } => {
                let file = OpenOptions::new()
                    .write(true)
                    .create(true)
                    .append(*append)
                    .truncate(!*append)
                    .open(path)
                    .map_err(|e| at(e, path))?;
                cmd.stdout(Stdio::from(file));
                false
            }
        };
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                stop_group(leader, &mut children).await;
                return Err(at(e, &stage.program));
            }
        };
        if leader.is_none() {
            leader = child.id();
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_reader(stderr, Stream::Stderr, tx.clone());
        }
        if piped_out {
            let stdout = child.stdout.take().expect("stdout was piped");
            match stage.stdout {
                StageStdout::Pipe => carry = Some(stdout),
                _ => spawn_reader(stdout, Stream::Stdout, tx.clone()),
            }
        }
        children.push(child);
    }
    drop(tx);

    // Every stage holds a stderr pipe until it exits, so the channel stays open for as long as
    // anything is running — which is what lets a cancel land on a silent `sleep`.
    let mut stopped = false;
    loop {
        tokio::select! {
            // Biased so a cancel is honoured even when the pipeline is producing output steadily.
            biased;
            _ = cancelled(&mut cancel) => {
                stopped = true;
                break;
            }
            chunk = rx.recv() => match chunk {
                Some((stream, bytes)) => {
                    if !on_chunk(stream, &bytes) {
                        stopped = true;
                        break;
                    }
                }
                None => break,
            },
        }
    }
    if stopped {
        stop_group(leader, &mut children).await;
    }
    let mut codes = Vec::with_capacity(children.len());
    for child in &mut children {
        codes.push(child.wait().await?.code());
    }
    Ok(codes)
}

/// Hand every chunk a pipe yields to `tx`, until it closes.
fn spawn_reader<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    mut pipe: R,
    stream: Stream,
    tx: tokio::sync::mpsc::UnboundedSender<(Stream, Vec<u8>)>,
) {
    use tokio::io::AsyncReadExt;
    tokio::spawn(async move {
        let mut buf = [0u8; 8192];
        loop {
            match pipe.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send((stream, buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
            }
        }
    });
}

/// Kill the group `leader` leads — `SIGTERM`, then `SIGKILL` after [`KILL_GRACE`] for anything
/// still there — and reap every child.
async fn stop_group(leader: Option<u32>, children: &mut [tokio::process::Child]) {
    let Some(pid) = leader else { return };
    kill_group(pid, libc_sigterm());
    let grace = tokio::time::sleep(KILL_GRACE);
    tokio::pin!(grace);
    let mut escalated = false;
    for child in children.iter_mut() {
        loop {
            tokio::select! {
                _ = child.wait() => break,
                _ = &mut grace, if !escalated => {
                    kill_group(pid, libc_sigkill());
                    escalated = true;
                }
            }
        }
    }
}

#[cfg(test)]
mod pipeline_tests {
    use super::*;
    use std::path::PathBuf;

    /// An executable by name, as the tests' own `PATH` finds it.
    fn exe(name: &str) -> PathBuf {
        std::env::var("PATH")
            .unwrap()
            .split(':')
            .map(|d| std::path::Path::new(d).join(name))
            .find(|p| p.is_file())
            .unwrap_or_else(|| panic!("{name} on PATH"))
    }

    fn stage(name: &str, args: &[&str], stdin: StageStdin, stdout: StageStdout) -> StageSpec {
        StageSpec {
            program: exe(name),
            args: args.iter().map(|s| s.to_string()).collect(),
            env: Vec::new(),
            stdin,
            stdout,
        }
    }

    async fn collect(stages: Vec<StageSpec>, cwd: &std::path::Path) -> (String, Vec<Option<i32>>) {
        let (_handle, token) = cancel_channel();
        let mut text = String::new();
        let codes = run_pipeline(
            stages,
            cwd,
            &std::collections::HashMap::new(),
            token,
            |_, c| {
                text.push_str(&String::from_utf8_lossy(c));
                true
            },
        )
        .await
        .unwrap();
        (text, codes)
    }

    #[tokio::test]
    async fn stages_are_wired_stdout_to_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let (text, codes) = collect(
            vec![
                stage("printf", &["b\na\n"], StageStdin::Null, StageStdout::Pipe),
                stage("sort", &[], StageStdin::Pipe, StageStdout::Pipe),
                stage("tr", &["a-z", "A-Z"], StageStdin::Pipe, StageStdout::Sink),
            ],
            dir.path(),
        )
        .await;
        assert_eq!(text, "A\nB\n");
        assert_eq!(codes, vec![Some(0), Some(0), Some(0)]);
    }

    #[tokio::test]
    async fn every_stages_stderr_reaches_the_sink_and_codes_are_per_stage() {
        let dir = tempfile::tempdir().unwrap();
        let (text, codes) = collect(
            vec![
                stage(
                    "sh",
                    &["-c", "echo oops >&2; exit 3"],
                    StageStdin::Null,
                    StageStdout::Pipe,
                ),
                stage("cat", &[], StageStdin::Pipe, StageStdout::Sink),
            ],
            dir.path(),
        )
        .await;
        assert_eq!(text, "oops\n");
        assert_eq!(codes, vec![Some(3), Some(0)]);
    }

    #[tokio::test]
    async fn redirections_open_files_relative_to_nothing_but_their_path() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.txt");
        collect(
            vec![stage(
                "printf",
                &["hi\n"],
                StageStdin::Null,
                StageStdout::File {
                    path: out.clone(),
                    append: false,
                },
            )],
            dir.path(),
        )
        .await;
        collect(
            vec![stage(
                "printf",
                &["more\n"],
                StageStdin::Null,
                StageStdout::File {
                    path: out.clone(),
                    append: true,
                },
            )],
            dir.path(),
        )
        .await;
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "hi\nmore\n");
        let (text, _) = collect(
            vec![stage(
                "cat",
                &[],
                StageStdin::File(out.clone()),
                StageStdout::Sink,
            )],
            dir.path(),
        )
        .await;
        assert_eq!(text, "hi\nmore\n");
    }

    #[tokio::test]
    async fn a_missing_program_is_an_error_naming_it() {
        let dir = tempfile::tempdir().unwrap();
        let (_handle, token) = cancel_channel();
        let err = run_pipeline(
            vec![StageSpec {
                program: dir.path().join("nope"),
                args: vec![],
                env: vec![],
                stdin: StageStdin::Null,
                stdout: StageStdout::Sink,
            }],
            dir.path(),
            &std::collections::HashMap::new(),
            token,
            |_, _| true,
        )
        .await
        .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(err.to_string().contains("nope"), "{err}");
    }

    #[tokio::test]
    async fn a_cancel_kills_every_stage() {
        let dir = tempfile::tempdir().unwrap();
        let (handle, token) = cancel_channel();
        let run = tokio::spawn({
            let cwd = dir.path().to_path_buf();
            async move {
                run_pipeline(
                    vec![
                        stage("sleep", &["100"], StageStdin::Null, StageStdout::Pipe),
                        stage("cat", &[], StageStdin::Pipe, StageStdout::Sink),
                    ],
                    &cwd,
                    &std::collections::HashMap::new(),
                    token,
                    |_, _| true,
                )
                .await
            }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        handle.send(true).unwrap();
        let codes = tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .expect("the cancel did not wait the sleep out")
            .unwrap()
            .unwrap();
        assert_eq!(codes, vec![None, None], "both killed by the signal");
    }
}

// ---- bytes to text ------------------------------------------------------------------------------

/// Where an ANSI escape sequence has got to. See [`OutputText::push`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Ansi {
    /// Not in a sequence.
    #[default]
    Ground,
    /// Seen `ESC`, waiting to find out what kind.
    Escape,
    /// Inside a `CSI` (`ESC [`) sequence, which ends at the first byte in `0x40..=0x7E`.
    Csi,
    /// Inside a string sequence (`OSC`, `DCS`, `APC`, …), which ends at `BEL` or `ESC \`.
    String,
    /// Seen `ESC` inside a string sequence: `\` ends it, anything else is more string.
    StringEsc,
}

/// A run's output as text a document can hold.
///
/// Three things happen between a pipe and a buffer line, and each of them is a bug if it is
/// skipped:
///
/// - **UTF-8 across chunks.** A read boundary lands mid-character often enough to matter; the
///   incomplete tail is carried into the next chunk rather than becoming a replacement glyph.
/// - **ANSI escapes.** `TERM=dumb` and `NO_COLOR=1` stop most tools colouring, but not all of
///   them, and a stray `ESC [ 0 m` in a buffer is unreadable rather than merely unstyled. A small
///   state machine strips CSI, the string sequences (OSC and friends) and the simple two-byte
///   forms — no dependency, and no attempt to *interpret* any of them, because this is not a
///   terminal.
/// - **Carriage returns.** `cargo`, `pip` and `npm` draw progress by rewriting one line with
///   `\r`. Appending them would make a build's transcript thousands of near-identical lines, so a
///   `\r` truncates back to the start of the current line — the same thing a terminal shows, which
///   is what the reader is expecting to see.
///
/// The result is append-mostly: everything before the current line's start is final, which is what
/// lets the shell flush its output as a tail write rather than rewriting the document.
#[derive(Debug, Default)]
pub struct OutputText {
    text: String,
    /// An incomplete UTF-8 sequence carried from the previous chunk.
    carry: Vec<u8>,
    esc: Ansi,
    /// A carriage return has arrived and we are waiting to see what follows it. `\n` makes the
    /// pair an ordinary line break (CRLF, which is what a Windows-flavoured tool writes); anything
    /// else means the line is being redrawn, and the redraw starts by clearing it.
    ///
    /// Deferred rather than acted on immediately because those two cases are the same byte, and
    /// truncating on sight turned every CRLF line into an empty one.
    pending_cr: bool,
    /// Byte index in `text` where the current (last) line starts — what a redraw truncates to.
    line_start: usize,
}

impl OutputText {
    /// Append a chunk of raw output.
    pub fn push(&mut self, bytes: &[u8]) {
        let decoded: String = if self.carry.is_empty() {
            match std::str::from_utf8(bytes) {
                Ok(s) => s.to_string(),
                Err(_) => self.decode_with_carry(bytes),
            }
        } else {
            self.decode_with_carry(bytes)
        };
        for c in decoded.chars() {
            self.push_char(c);
        }
    }

    /// Decode `bytes` preceded by whatever partial character the last chunk ended on, keeping any
    /// new partial tail for the next one. A sequence that is *invalid* rather than merely
    /// unfinished becomes the replacement character, as `from_utf8_lossy` would.
    fn decode_with_carry(&mut self, bytes: &[u8]) -> String {
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(bytes);
        let keep = incomplete_suffix_start(&buf);
        self.carry = buf[keep..].to_vec();
        String::from_utf8_lossy(&buf[..keep]).into_owned()
    }

    fn push_char(&mut self, c: char) {
        match self.esc {
            Ansi::Ground => match c {
                '\u{1b}' => self.esc = Ansi::Escape,
                // A second `\r` before anything printable is still one redraw.
                '\r' => self.pending_cr = true,
                '\n' => {
                    self.pending_cr = false;
                    self.text.push('\n');
                    self.line_start = self.text.len();
                }
                '\t' => {
                    self.redraw_if_pending();
                    self.text.push('\t');
                }
                // Every other C0 control (BEL, backspace, form feed) is terminal choreography we
                // are not performing; dropping it keeps the text readable.
                c if (c as u32) < 0x20 || c == '\u{7f}' => {}
                c => {
                    self.redraw_if_pending();
                    self.text.push(c);
                }
            },
            Ansi::Escape => {
                self.esc = match c {
                    '[' => Ansi::Csi,
                    // OSC, DCS, SOS, PM, APC — all string sequences ended by BEL or `ESC \`.
                    ']' | 'P' | 'X' | '^' | '_' => Ansi::String,
                    // A simple two-byte sequence (`ESC c`, `ESC 7`, …): consumed whole.
                    _ => Ansi::Ground,
                };
            }
            Ansi::Csi => {
                // Parameter and intermediate bytes, then one final byte in `0x40..=0x7e`.
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    self.esc = Ansi::Ground;
                }
            }
            Ansi::String => {
                self.esc = match c {
                    '\u{7}' => Ansi::Ground,
                    '\u{1b}' => Ansi::StringEsc,
                    _ => Ansi::String,
                };
            }
            Ansi::StringEsc => {
                self.esc = if c == '\\' {
                    Ansi::Ground
                } else {
                    Ansi::String
                };
            }
        }
    }

    /// Act on a deferred carriage return: the line is being redrawn, so clear it first.
    fn redraw_if_pending(&mut self) {
        if std::mem::take(&mut self.pending_cr) {
            self.text.truncate(self.line_start);
        }
    }

    /// Make sure the text ends with a newline, so the next run starts on a fresh line. A run with
    /// no output at all contributes one empty line, which is what keeps every element of a shell
    /// view non-empty.
    pub fn end_line(&mut self) {
        self.pending_cr = false;
        if !self.text.ends_with('\n') {
            self.text.push('\n');
            self.line_start = self.text.len();
        }
    }

    /// Append a line of our own — the truncation marker, and nothing else so far.
    pub fn push_line(&mut self, line: &str) {
        self.end_line();
        self.text.push_str(line);
        self.end_line();
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Bytes of text held. Bytes rather than the bytes *read*: what matters downstream is how much
    /// document there is.
    pub fn bytes(&self) -> usize {
        self.text.len()
    }

    /// Byte index where the current (last) line starts.
    ///
    /// The one thing a flush has to record: everything before it is **final** — only the last line
    /// can still be redrawn by a carriage return — so the next flush rewrites the document from
    /// here and nothing earlier. It only ever moves forward, which is what makes it safe to use as
    /// a rewrite point against the *older* text a previous flush left in the document.
    pub fn line_start(&self) -> usize {
        self.line_start
    }
}

/// Where an *unfinished* UTF-8 sequence begins at the end of `bytes` — `bytes.len()` when the tail
/// is complete (or broken rather than merely unfinished, which is the lossy decoder's problem, not
/// a reason to wait for more bytes that will never make it valid).
///
/// Walked from the end rather than read off a `Utf8Error`, because an invalid byte earlier in the
/// chunk makes the error describe *that* instead, and the tail would then be decoded twice.
fn incomplete_suffix_start(bytes: &[u8]) -> usize {
    let n = bytes.len();
    // The longest sequence is four bytes, so only the last three can be unfinished.
    for back in 1..=3.min(n) {
        let i = n - back;
        let b = bytes[i];
        if b < 0x80 {
            break; // ASCII: whatever follows is its own business.
        }
        if b & 0xC0 == 0x80 {
            continue; // A continuation byte; its lead is further back.
        }
        let need = match b {
            b if b & 0xE0 == 0xC0 => 2,
            b if b & 0xF0 == 0xE0 => 3,
            b if b & 0xF8 == 0xF0 => 4,
            _ => 1, // Not a lead byte at all — invalid, and the lossy pass says so.
        };
        return if back < need { i } else { n };
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn sh(script: &str, cwd: &Path) -> Command {
        let mut cmd = command("/bin/sh");
        cmd.arg("-c").arg(script).current_dir(cwd);
        cmd
    }

    /// Both pipes are read, and their chunks arrive in the order the child wrote them.
    ///
    /// The sleeps are the test, not padding: without them the three writes land in the pipes
    /// before the reader is ever scheduled and the OS decides the order, which would make this
    /// assert about buffering rather than about the runner.
    #[tokio::test]
    async fn both_streams_are_reported_in_arrival_order() {
        let dir = tempfile::tempdir().unwrap();
        let (_handle, token) = cancel_channel();
        let mut seen: Vec<(Stream, String)> = Vec::new();
        let exit = run_streaming(
            sh(
                "printf 'one\\n'; sleep 0.2; printf 'two\\n' >&2; sleep 0.2; printf 'three\\n'",
                dir.path(),
            ),
            token,
            |stream, bytes| {
                seen.push((stream, String::from_utf8_lossy(bytes).into_owned()));
                true
            },
        )
        .await
        .expect("sh ran");
        assert_eq!(exit.code, Some(0));
        let joined: String = seen.iter().map(|(_, s)| s.as_str()).collect();
        assert_eq!(joined, "one\ntwo\nthree\n", "seen: {seen:?}");
        assert!(
            seen.iter().any(|(s, _)| *s == Stream::Stderr),
            "stderr must be labelled as such: {seen:?}"
        );
    }

    /// A non-zero exit travels intact, so a run's header can say `exit 3` rather than "failed".
    #[tokio::test]
    async fn exit_codes_travel_intact() {
        let dir = tempfile::tempdir().unwrap();
        let (_handle, token) = cancel_channel();
        let exit = run_streaming(sh("exit 3", dir.path()), token, |_, _| true)
            .await
            .unwrap();
        assert_eq!(exit.code, Some(3));
    }

    /// Cancelling kills the **group**, not merely the shell — a `sleep` started by the script is
    /// what a build would be, and leaving it running is the failure this exists to prevent.
    ///
    /// Proven by the file the survivor would write: if the grandchild outlives the cancel it
    /// creates `alive`, and the assertion is that it never does.
    #[tokio::test]
    async fn a_cancelled_run_kills_the_whole_group() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("alive");
        let script = format!(
            "( sleep 3; : > {} ) & printf 'started\\n'; wait",
            marker.display()
        );
        let (handle, token) = cancel_channel();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            let _ = handle.send(true);
        });
        let started = std::time::Instant::now();
        let exit = tokio::time::timeout(
            Duration::from_secs(10),
            run_streaming(sh(&script, dir.path()), token, |_, _| true),
        )
        .await
        .expect("returned rather than waiting the sleep out")
        .expect("sh ran");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "the cancel must not have waited for the sleep"
        );
        // Killed by a signal, so there is no code — which is why callers read cancellation off
        // their token rather than out of the status.
        assert_eq!(exit.code, None);
        // Give the survivor more than its own sleep to prove it isn't there.
        tokio::time::sleep(Duration::from_millis(3200)).await;
        assert!(
            !marker.exists(),
            "the grandchild outlived the cancel — the group was not killed"
        );
    }

    /// A sink that says "enough" stops the run, which is how a shell view caps one command's
    /// output without the cap living somewhere that can be forgotten.
    #[tokio::test]
    async fn a_sink_can_stop_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let (_handle, token) = cancel_channel();
        let mut bytes = 0usize;
        let exit = tokio::time::timeout(
            Duration::from_secs(20),
            // `yes` never ends on its own: only the sink's refusal can stop this.
            run_streaming(sh("yes", dir.path()), token, |_, chunk| {
                bytes += chunk.len();
                bytes < 64 * 1024
            }),
        )
        .await
        .expect("the sink's refusal stopped the run")
        .expect("sh ran");
        assert_eq!(exit.code, None, "killed rather than allowed to finish");
        assert!(bytes >= 64 * 1024);
    }

    /// The end-to-end shape the shell view relies on: `yes | head` finishes on its own, and the
    /// text arrives as lines.
    #[tokio::test]
    async fn a_pipeline_that_ends_itself_is_collected_whole() {
        let dir = tempfile::tempdir().unwrap();
        let (_handle, token) = cancel_channel();
        let mut out = OutputText::default();
        let exit = run_streaming(sh("yes | head -n 2000", dir.path()), token, |_, chunk| {
            out.push(chunk);
            true
        })
        .await
        .unwrap();
        assert_eq!(exit.code, Some(0));
        assert_eq!(out.text().matches('\n').count(), 2000);
        assert_eq!(out.text().lines().next(), Some("y"));
    }

    /// A real `\r` progress line, straight out of a shell: one line, not three.
    #[tokio::test]
    async fn a_carriage_return_progress_line_stays_one_line() {
        let dir = tempfile::tempdir().unwrap();
        let (_handle, token) = cancel_channel();
        let mut out = OutputText::default();
        run_streaming(
            sh(
                "printf 'Building 10%%\\rBuilding 60%%\\rBuilding 100%%\\ndone\\n'",
                dir.path(),
            ),
            token,
            |_, chunk| {
                out.push(chunk);
                true
            },
        )
        .await
        .unwrap();
        assert_eq!(out.text(), "Building 100%\ndone\n");
    }

    #[test]
    fn a_carriage_return_rewrites_its_line_from_the_start() {
        let mut out = OutputText::default();
        out.push(b"keep\nprogress 10%");
        assert_eq!(out.text(), "keep\nprogress 10%");
        out.push(b"\rprogress 90%");
        assert_eq!(
            out.text(),
            "keep\nprogress 90%",
            "only the last line is rewritten"
        );
        out.push(b"\rdone\n");
        assert_eq!(out.text(), "keep\ndone\n");
        // A `\r\n` pair is one line break, not a wipe followed by a break — CRLF output would
        // otherwise come out as a column of empty lines.
        let mut out = OutputText::default();
        out.push(b"a\r\nb\r\n");
        assert_eq!(out.text(), "a\nb\n");
        // And the `\r` may be the last byte of a chunk, which is how it usually arrives.
        let mut out = OutputText::default();
        out.push(b"10%\r");
        out.push(b"90%\n");
        assert_eq!(out.text(), "90%\n");
    }

    #[test]
    fn ansi_escapes_are_stripped_not_shown() {
        let mut out = OutputText::default();
        // SGR colour, a cursor move, an OSC title ended by BEL, an OSC ended by ST, and a
        // two-byte `ESC c`.
        out.push(b"\x1b[31mred\x1b[0m\x1b[2Kclear\x1b]0;title\x07t\x1b]8;;u\x1b\\link\x1bcreset\n");
        assert_eq!(out.text(), "redcleartlinkreset\n");
        // Split across chunk boundaries mid-sequence, which is how they actually arrive.
        let mut out = OutputText::default();
        out.push(b"a\x1b[3");
        out.push(b"1mb\n");
        assert_eq!(out.text(), "ab\n");
    }

    #[test]
    fn utf8_is_carried_across_chunk_boundaries() {
        let mut out = OutputText::default();
        let text = "héllo → wörld\n";
        let bytes = text.as_bytes();
        // Every split point, one at a time, is a boundary the decoder has to survive.
        for split in 1..bytes.len() {
            let mut out = OutputText::default();
            out.push(&bytes[..split]);
            out.push(&bytes[split..]);
            assert_eq!(out.text(), text, "split at {split}");
        }
        // Genuinely invalid bytes are replaced rather than swallowing the rest of the line.
        out.push(b"a\xffb\n");
        assert!(out.text().starts_with('a') && out.text().ends_with("b\n"));
    }

    /// What the shell's tail write rewrites from, and the property that makes it safe: the start
    /// of the current line only ever moves forward, so a point recorded at one flush is still a
    /// valid rewrite point at the next — even when a carriage return has since made the text
    /// *shorter* than it was.
    #[test]
    fn the_line_start_only_moves_forward() {
        let mut out = OutputText::default();
        assert_eq!(out.line_start(), 0);
        out.push(b"one\n");
        assert_eq!(out.line_start(), 4);
        out.push(b"progress 10%");
        assert_eq!(out.line_start(), 4, "still inside the same line");
        let before = out.bytes();
        out.push(b"\r90%");
        assert!(out.bytes() < before, "the redraw made the text shorter");
        assert_eq!(out.line_start(), 4, "and the rewrite point held");
        out.push(b"\ntwo");
        assert_eq!(out.line_start(), 8);
        out.end_line();
        assert_eq!(out.line_start(), out.bytes());
    }

    /// Every run ends on a newline, and a run that said nothing still contributes one line — the
    /// property that keeps every element of a shell view non-empty.
    #[test]
    fn end_line_leaves_one_line_even_for_silence() {
        let mut out = OutputText::default();
        out.end_line();
        assert_eq!(
            out.text(),
            "\n",
            "a run that said nothing is still one empty line"
        );
        out.push(b"tail");
        out.end_line();
        assert_eq!(out.text(), "\ntail\n");
        out.end_line();
        assert_eq!(out.text(), "\ntail\n", "idempotent");
        out.push_line("[output truncated]");
        assert_eq!(out.text(), "\ntail\n[output truncated]\n");
    }

    /// The classification, stated as the two mistakes worth making: dropping a variable the user
    /// set, and keeping one cargo set.
    #[test]
    fn build_context_is_what_cargo_injected_and_nothing_else() {
        for injected in [
            "OUT_DIR",
            "CARGO",
            "CARGO_MANIFEST_DIR",
            "CARGO_MANIFEST_PATH",
            "CARGO_PKG_NAME",
            "CARGO_PKG_VERSION_PRE",
            "CARGO_BIN_EXE_ae",
            "RUST_RECURSION_COUNT",
        ] {
            assert!(
                is_build_context(injected),
                "{injected} is cargo's, not the user's"
            );
        }
        for kept in [
            // Ordinary user configuration a child running cargo still needs.
            "CARGO_HOME",
            "RUSTUP_HOME",
            // How the toolchain that built us names itself to its children; see `is_build_context`.
            "RUSTUP_TOOLCHAIN",
            "RUSTUP_TOOLCHAIN_SOURCE",
            "PATH",
            "HOME",
            "SHELL",
        ] {
            assert!(!is_build_context(kept), "{kept} is the user's, not cargo's");
        }
    }

    /// The list cannot be checked against a document, so check it against the live article: this
    /// test runs under `cargo test`, which injects the same family `cargo run` does. Anything
    /// cargo-shaped in our own environment that [`is_build_context`] does not claim is either a
    /// variable cargo started setting since this was written — in which case find out whether a
    /// build script watches it and add it — or a user variable that wants naming in `keep` below.
    #[test]
    fn the_list_still_covers_what_cargo_injects_here() {
        // Everything cargo-shaped that is legitimately the user's: `CARGO_HOME`, and cargo's
        // configuration-by-environment (`CARGO_<SECTION>_<KEY>`), which cargo reads but never
        // injects. CI sets some of these — `Swatinem/rust-cache` exports `CARGO_INCREMENTAL=0` —
        // and a child running cargo should inherit them like any other config.
        const KEEP: &[&str] = &[
            "CARGO_HOME",
            "CARGO_INCREMENTAL",
            "CARGO_TARGET_DIR",
            "CARGO_LOG",
        ];
        const KEEP_PREFIXES: &[&str] = &[
            "CARGO_BUILD_",
            "CARGO_TERM_",
            "CARGO_NET_",
            "CARGO_HTTP_",
            "CARGO_REGISTRY_",
            "CARGO_REGISTRIES_",
            "CARGO_PROFILE_",
        ];
        let missed: Vec<String> = std::env::vars()
            .map(|(k, _)| k)
            .filter(|k| k.starts_with("CARGO") || k == "OUT_DIR" || k == "RUST_RECURSION_COUNT")
            .filter(|k| {
                !KEEP.contains(&k.as_str()) && !KEEP_PREFIXES.iter().any(|p| k.starts_with(p))
            })
            .filter(|k| !is_build_context(k))
            .collect();
        assert!(
            missed.is_empty(),
            "cargo set {missed:?} in this process and `is_build_context` does not claim them — \
             a build script watching one of these would rebuild on every alternation between a \
             child of ours and the user's terminal"
        );
    }
}
