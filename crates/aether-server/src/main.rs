use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "aether", version, about = "Aether editor server daemon")]
struct Cli {
    /// Project name (must match a file in $XDG_CONFIG_HOME/aether/projects/<name>.toml)
    project: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("aether_server=info,info")),
        )
        .init();

    aether_server::run(&cli.project).await
}
