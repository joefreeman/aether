//! `aether-tui` — the terminal client, driven by the shared `aether-client` core. Owns the
//! crossterm terminal lifecycle (raw mode, alt-screen, kitty keyboard flags) and hands control to
//! [`shell::run`]; [`run`] is the single entry point the `ae` binary calls for the terminal client.

mod app;
/// CLI path resolution, shared with the `ae` binary: `ae --web`'s waiter resolves its file
/// argument exactly like the shells' boot opens so the same launch lands on the same buffer.
pub use app::resolve_cli_path;
mod clipboard;
mod connection;
mod labels;
mod overlay_input;
mod picker;
mod save_prompt;
mod scroll;
mod shell;
mod stderr_capture;
mod text_input;
mod ui;

use crossterm::cursor::SetCursorStyle;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{stdout, Stdout};
use tracing_subscriber::fmt::writer::BoxMakeWriter;

/// Run the terminal client to completion. `workspace`/`file` are the (optional) CLI positionals,
/// `tether` marks the quick-edit invocation (file positional, no explicit `--workspace` — the
/// opened buffer tethers the client), `version` is the handshake version string, and `server_url`
/// is the (profile-resolved) WebSocket address to dial; the caller (`ae`) parses these and provides
/// the tokio runtime this is awaited on.
pub async fn run(
    workspace: Option<String>,
    file: Option<String>,
    jump: Option<(u32, u32)>,
    tether: bool,
    version: String,
    server_url: String,
) -> anyhow::Result<()> {
    // Capture stderr for the lifetime of the program so log/panic/library output never lands
    // mid-frame on the alt-screen TUI. The capture is replayed to the real stderr on drop, which
    // happens *after* `restore_terminal` thanks to the variable's late drop order at the end of
    // this function.
    let _stderr_capture = stderr_capture::StderrCapture::install().ok();

    // Tracing writes to (captured) stderr, so the user sees logs only after the editor exits.
    // `AETHER_LOG_FILE` redirects them to a path instead, where they can be read *while* the
    // editor runs — the only way to watch the client live, since the TUI owns the terminal and a
    // stray line would land mid-frame. This is the client's single subscriber install: adding a
    // second one anywhere upstream panics on the global dispatcher, so route new sinks here.
    let log_file =
        std::env::var_os("AETHER_LOG_FILE").and_then(|path| std::fs::File::create(path).ok());
    let to_file = log_file.is_some();
    let writer = match log_file {
        Some(file) => BoxMakeWriter::new(std::sync::Mutex::new(file)),
        None => BoxMakeWriter::new(std::io::stderr),
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new(if to_file {
                    "aether_tui=debug,aether_client=debug,warn"
                } else {
                    "aether_tui=info,warn"
                })
            }),
        )
        .with_ansi(!to_file)
        .with_writer(writer)
        .init();

    let mut terminal = setup_terminal()?;
    install_panic_hook();

    // Launch connectionless: the editor chrome comes up immediately in a `Connecting` state
    // (status row showing "Connecting…", client-side keys live) and `run` dials `server_url` from
    // within — so the client can start before the daemon and waits for it without leaving the
    // editor. The boot dial installs the session once it lands.
    let run_result = shell::run(
        &mut terminal,
        workspace,
        file,
        jump,
        tether,
        version,
        server_url,
    )
    .await;
    restore_terminal(&mut terminal)?;
    run_result
}

fn setup_terminal() -> anyhow::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut out = stdout();
    // Bracketed paste: terminal-level pastes (middle-click, the terminal's own paste shortcut)
    // arrive as one `Event::Paste` instead of replayed keystrokes — replayed, a Normal-mode paste
    // runs the clipboard as commands, and an Insert-mode one feeds every newline through
    // auto-indent (the staircase). Terminals without support ignore the sequence.
    execute!(
        out,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    // Best-effort: enable the kitty keyboard protocol so the terminal disambiguates things like
    // Ctrl-Shift-Z and Alt-0. Terminals that don't support it ignore the escape sequence.
    let _ = execute!(
        out,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
        )
    );
    let backend = CrosstermBackend::new(out);
    Ok(Terminal::new(backend)?)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> anyhow::Result<()> {
    // Input modes off and the queue drained *before* raw mode goes: out of raw mode the tty echoes
    // whatever arrives, so a mouse report still in flight printed as `^[[<35;198;50M` and the
    // shell then read the rest as a command.
    let _ = execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        DisableBracketedPaste,
        PopKeyboardEnhancementFlags
    );
    drain_terminal_input();
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        SetCursorStyle::DefaultUserShape,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    Ok(())
}

/// How long [`drain_terminal_input`] waits for the terminal to answer its fence. Every terminal
/// answers a device-attributes query at once; this only bounds exit on one that never does.
const DRAIN_FENCE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

/// Discard every byte the terminal sent us that we never read. Call it with raw mode still on, and
/// after mouse capture is disabled.
///
/// Mouse tracking (on for the whole session, including the pre-connection "Connecting…" splash)
/// streams motion reports, and the ones that outlive us land in the shell as a garbled command.
/// Flushing the input queue alone isn't enough: a report the terminal wrote just before it read
/// our "disable" is still in the pty when the flush runs, and arrives after it. So fence first —
/// ask for the primary device attributes and read until the answer comes back. The terminal
/// answers in order, so once the reply is in, everything it sent before the disable is too. Safe:
/// a raw-mode full-screen app accumulates no shell-bound type-ahead, so there's nothing
/// legitimate to preserve.
fn drain_terminal_input() {
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;

    let tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty");
    if let Ok(mut tty) = tty {
        if tty.write_all(b"\x1b[c").and_then(|()| tty.flush()).is_ok() {
            let deadline = std::time::Instant::now() + DRAIN_FENCE_TIMEOUT;
            let mut seen = Vec::new();
            let mut buf = [0u8; 512];
            loop {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                let mut fd = libc::pollfd {
                    fd: tty.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: one valid, initialised pollfd, and its count.
                let ready = unsafe { libc::poll(&mut fd, 1, left.as_millis() as libc::c_int) };
                if ready <= 0 {
                    break;
                }
                match tty.read(&mut buf) {
                    Ok(n) if n > 0 => seen.extend_from_slice(&buf[..n]),
                    _ => break,
                }
                if ends_with_device_attributes(&seen) {
                    break;
                }
            }
        }
    }
    // SAFETY: `tcflush` on the stdin fd is a simple libc call with no memory effects; TCIFLUSH
    // discards unread input only (never our already-written output).
    unsafe {
        libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
    }
}

/// Whether `input` ends in a primary device attributes reply, `ESC [ ? Ps ; … c`. Nothing else a
/// terminal sends us has that shape: SGR mouse reports are `ESC [ <`, and the kitty flags reply
/// ends in `u`.
fn ends_with_device_attributes(input: &[u8]) -> bool {
    let Some((b'c', body)) = input.split_last() else {
        return false;
    };
    let params = body
        .iter()
        .rev()
        .take_while(|b| b.is_ascii_digit() || **b == b';')
        .count();
    body[..body.len() - params].ends_with(b"\x1b[?")
}

fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Same order as `restore_terminal`: drained before raw mode goes, or the tty echoes it.
        let _ = execute!(
            stdout(),
            DisableMouseCapture,
            DisableBracketedPaste,
            PopKeyboardEnhancementFlags
        );
        drain_terminal_input();
        let _ = disable_raw_mode();
        let _ = execute!(
            stdout(),
            SetCursorStyle::DefaultUserShape,
            LeaveAlternateScreen
        );
        original(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::ends_with_device_attributes;

    #[test]
    fn a_device_attributes_reply_ends_the_drain() {
        assert!(ends_with_device_attributes(b"\x1b[?62;22c"));
        assert!(ends_with_device_attributes(b"\x1b[<35;198;50M\x1b[?65;1;9c"));
        assert!(ends_with_device_attributes(b"\x1b[?c"));
    }

    #[test]
    fn other_replies_do_not() {
        // Still waiting: nothing yet, a mouse report, a kitty flags reply, a reply cut short.
        assert!(!ends_with_device_attributes(b""));
        assert!(!ends_with_device_attributes(b"c"));
        assert!(!ends_with_device_attributes(b"\x1b[<35;198;50M"));
        assert!(!ends_with_device_attributes(b"\x1b[?1u"));
        assert!(!ends_with_device_attributes(b"\x1b[?62;22"));
        // A mouse report's parameters followed by a stray `c` isn't a reply either.
        assert!(!ends_with_device_attributes(b"\x1b[<35;1c"));
    }
}
