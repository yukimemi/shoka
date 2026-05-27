//! shoka — your repository bookshelf.

pub(crate) mod actions;
pub mod cache;
pub mod cli;
pub mod commands;
pub mod config;
pub mod gh;
pub mod git_status;
pub mod paths;
pub mod remote;
pub mod state;
pub mod updater;
pub mod vcs;

/// Install the `aws_lc_rs` rustls `CryptoProvider` as the process
/// default. Both reqwest and octocrab pull rustls 0.23 into the
/// build with conflicting `CryptoProvider` features enabled, so
/// rustls refuses to auto-pick one and panics the first time any
/// HTTPS handshake runs unless this is called first. Call it as
/// early as possible in `main()` — before any code path that
/// touches HTTPS (`shoka cache refresh`, gh snapshot, self-update,
/// clone).
///
/// The `Result` is intentionally discarded: `install_default()` is
/// idempotent in the success case but returns `Err` when *some
/// other* library has already claimed the slot. Racing-and-losing
/// silently is the right behavior — we just need *a* provider
/// installed, and the other library's choice is fine if it got
/// there first.
pub fn install_default_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_default_crypto_provider_makes_global_available() {
        // Regression test for the startup panic that hit
        // `shoka cache refresh` after v0.10.0: rustls 0.23 with both
        // `aws-lc-rs` *and* `ring` features pulled in cannot auto-
        // pick a `CryptoProvider`. After this call, the global slot
        // must be filled; otherwise any subsequent HTTPS handshake
        // panics — exactly what users hit before this fix.
        install_default_crypto_provider();
        assert!(
            rustls::crypto::CryptoProvider::get_default().is_some(),
            "global crypto provider must be installed after install_default_crypto_provider()"
        );
    }
}
