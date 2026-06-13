mod app;
mod clipboard;
mod connection;
mod discovery;
mod labels;
mod picker;
mod save_prompt;
mod scroll;
mod shell;
mod stderr_capture;
mod text_input;
mod ui;

use clap::Parser;
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

#[derive(Parser, Debug)]
#[command(name = "ae", version, about = "Aether editor — terminal client")]
struct Cli {
    /// Project name. Optional — omit to start with the project picker open. The named project
    /// must have a config at `$XDG_CONFIG_HOME/aether/projects/<name>.toml`; the daemon loads
    /// it on first activation.
    project: Option<String>,
    /// File or directory to open. Resolved against the current working directory and must fall
    /// within one of the project's roots. A directory opens the file browser at that location
    /// with a scratch buffer underneath. Omit to start in a scratch buffer with no file browser.
    /// Ignored when no project is selected.
    file: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Capture stderr for the lifetime of the program so log/panic/library output never lands
    // mid-frame on the alt-screen TUI. The capture is replayed to the real stderr on drop
    // (at process exit), which happens *after* `restore_terminal` thanks to the variable's
    // late drop order at the end of `main`.
    let _stderr_capture = stderr_capture::StderrCapture::install().ok();

    // Tracing writes to (captured) stderr. The user sees logs after the editor exits.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("aether_tui=info,warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let info = discovery::read()?;
    let base_url = format!("ws://127.0.0.1:{}", info.port);
    let (handle, notifications) = connection::connect(&base_url, env!("CARGO_PKG_VERSION")).await?;

    let mut terminal = setup_terminal()?;
    install_panic_hook();

    let (cols, rows) = crossterm::terminal::size()?;
    let (session, state, startup) = match shell::bootstrap(
        &handle,
        cli.project.as_deref(),
        cli.file.as_deref(),
        cols,
        rows,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            restore_terminal(&mut terminal).ok();
            return Err(e);
        }
    };

    let run_result =
        shell::run(&mut terminal, handle, notifications, session, state, startup).await;
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
