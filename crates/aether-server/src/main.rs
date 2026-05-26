use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "aether", version, about = "Aether editor server daemon")]
struct Cli {}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("aether_server=info,info")),
        )
        .init();

    aether_server::run().await
}
