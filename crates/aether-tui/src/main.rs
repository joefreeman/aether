mod app;
mod client;
mod clipboard;
mod discovery;
mod stderr_capture;
mod ui;

use clap::Parser;
use crossterm::cursor::SetCursorStyle;
use crossterm::event::{
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
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
    /// Project name (looks up the running server in $XDG_RUNTIME_DIR/aether/)
    project: String,
    /// File to open, relative to the first project path. Omit to open a scratch buffer.
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

    let info = discovery::read(&cli.project)?;
    let url = format!("ws://127.0.0.1:{}", info.port);
    let mut client = client::Client::connect(&url).await?;

    let mut terminal = setup_terminal()?;
    install_panic_hook();

    let (cols, rows) = crossterm::terminal::size()?;
    let mut state = match app::bootstrap(&mut client, info.token, cli.file.as_deref(), cols, rows).await {
        Ok(s) => s,
        Err(e) => {
            restore_terminal(&mut terminal).ok();
            return Err(e);
        }
    };

    let run_result = app::run(&mut terminal, &mut client, &mut state).await;
    restore_terminal(&mut terminal)?;
    run_result
}

fn setup_terminal() -> anyhow::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen)?;
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
    execute!(terminal.backend_mut(), SetCursorStyle::DefaultUserShape, LeaveAlternateScreen)?;
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
            SetCursorStyle::DefaultUserShape,
            LeaveAlternateScreen
        );
        original(info);
    }));
}
