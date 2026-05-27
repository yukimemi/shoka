//! Release-time HTTPS smoke test for shoka.
//!
//! `release.yml` runs this example on every build matrix entry after
//! `cargo build --release`. It exercises shoka's actual startup path
//! and forces a real rustls handshake — the regression class that
//! shipped as v0.10.0's `CryptoProvider` panic (caught only when users
//! ran `shoka cache refresh`, never in CI).
//!
//! The two checks are deliberately separate concerns:
//!
//! 1. `install_default_crypto_provider()` is called the same way
//!    `main.rs` does, so any regression to that helper itself shows
//!    up first.
//! 2. An octocrab unauthenticated GET against the canonical
//!    `octocat/Hello-World` repo forces the rustls handshake to
//!    actually run in the produced binary — this is the step that
//!    would have caught v0.10.0's bug before crates.io publish.
//!
//! Cost: ~5 seconds per platform per release plus a single
//! authenticated GitHub API request — `$GITHUB_TOKEN` lets us share
//! the runner's per-repo budget (~1000 reqs/h) instead of leaning on
//! the anonymous limit, which is per-runner-IP and routinely
//! exhausted by other jobs on shared Actions infrastructure (a
//! release that hits 403 blocks shipping for no real reason). Local
//! invocations without `$GITHUB_TOKEN` fall back to anonymous, which
//! is fine for the dev loop's much lower request rate.

use anyhow::{Context, Result};

#[tokio::main]
async fn main() -> Result<()> {
    shoka::install_default_crypto_provider();

    let mut builder = octocrab::OctocrabBuilder::new();
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        if !token.is_empty() {
            builder = builder.personal_token(token);
        }
    }
    let octo = builder.build().context("octocrab client init")?;

    let repo = octo
        .repos("octocat", "Hello-World")
        .get()
        .await
        .context("fetching octocat/Hello-World over HTTPS")?;

    println!(
        "smoke OK: rustls handshake completed, fetched {} (id={})",
        repo.full_name.as_deref().unwrap_or("octocat/Hello-World"),
        repo.id
    );
    Ok(())
}
