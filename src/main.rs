use clap::Parser;
use shoka::cli::Cli;
use shoka::commands;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Install the rustls crypto provider before anything that might
    // open an HTTPS connection — see [`shoka::install_default_crypto_provider`]
    // for the full rationale. Must come before the tracing init too
    // (defensive: tracing-subscriber doesn't touch HTTPS, but
    // anything else that drops in later might).
    shoka::install_default_crypto_provider();

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
