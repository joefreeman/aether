mod app;
mod client;
mod discovery;
mod ui;

use clap::Parser;
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

    // tracing logs go to stderr; not visible while in the alt screen but useful for debugging
    // when invoked with `2>>/tmp/ae.log`.
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
    let backend = CrosstermBackend::new(out);
    Ok(Terminal::new(backend)?)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> anyhow::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), LeaveAlternateScreen);
        original(info);
    }));
}
