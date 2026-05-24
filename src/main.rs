use clap::Parser;
use shoka::cli::Cli;
use shoka::commands;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("SHOKA_LOG")
                .unwrap_or_else(|_| EnvFilter::new("warn,shoka=info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    commands::dispatch(cli).await
}
