//! `aether-tui` — the terminal client, driven by the shared `aether-client` core. Owns the
//! crossterm terminal lifecycle (raw mode, alt-screen, kitty keyboard flags) and hands control to
//! [`shell::run`]; [`run`] is the single entry point the `ae` binary calls for the terminal client.

mod app;
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
    DisableMouseCapture, EnableMouseCapture, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{stdout, Stdout};

/// Run the terminal client to completion. `workspace`/`file` are the (optional) CLI positionals,
/// `version` is the handshake version string, and `server_url` is the (profile-resolved) WebSocket
/// address to dial; the caller (`ae`) parses these and provides the tokio runtime this is awaited on.
pub async fn run(
    workspace: Option<String>,
    file: Option<String>,
    jump: Option<(u32, u32)>,
    version: String,
    server_url: String,
) -> anyhow::Result<()> {
    // Capture stderr for the lifetime of the program so log/panic/library output never lands
    // mid-frame on the alt-screen TUI. The capture is replayed to the real stderr on drop, which
    // happens *after* `restore_terminal` thanks to the variable's late drop order at the end of
    // this function.
    let _stderr_capture = stderr_capture::StderrCapture::install().ok();

    // Tracing writes to (captured) stderr. The user sees logs after the editor exits.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("aether_tui=info,warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let mut terminal = setup_terminal()?;
    install_panic_hook();

    // Launch connectionless: the editor chrome comes up immediately in a `Connecting` state
    // (status row showing "Connecting…", client-side keys live) and `run` dials `server_url` from
    // within — so the client can start before the daemon and waits for it without leaving the
    // editor. The boot dial installs the session once it lands.
    let run_result = shell::run(&mut terminal, workspace, file, jump, version, server_url).await;
    restore_terminal(&mut terminal)?;
    run_result
}

fn setup_terminal() -> anyhow::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
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
    disable_raw_mode()?;
    let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
    let _ = execute!(terminal.backend_mut(), DisableMouseCapture);
    execute!(
        terminal.backend_mut(),
        SetCursorStyle::DefaultUserShape,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    Ok(())
}

fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(
            stdout(),
            PopKeyboardEnhancementFlags,
            DisableMouseCapture,
            SetCursorStyle::DefaultUserShape,
            LeaveAlternateScreen
        );
        original(info);
    }));
}
