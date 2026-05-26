//! Self-update support via the [`kaishin`] crate — same engine
//! [yukimemi/rvpm][rvpm] and [yukimemi/renri][renri] use, so the UX
//! (download glyph, version-skip prompt, `--check` rehearsal mode)
//! is consistent across the yukimemi/* CLI fleet.
//!
//! Resolves the latest release from `github.com/yukimemi/shoka`,
//! downloads the asset matching the current platform, swaps it over
//! the running binary, and exits. The user can re-invoke against
//! the new version immediately afterwards — `cargo install` style.
//!
//! shoka's main entry point is already async (it uses tokio), so
//! unlike renri this module exposes a plain async function rather
//! than wrapping a fresh runtime. The dispatcher's existing tokio
//! context carries it.
//!
//! [`kaishin`]: https://github.com/yukimemi/kaishin
//! [rvpm]: https://github.com/yukimemi/rvpm
//! [renri]: https://github.com/yukimemi/renri

use anyhow::Result;

/// Run the self-update flow.
///
/// - `yes`: skip the "install vX.Y.Z?" confirmation prompt.
/// - `check_only`: print availability and exit without downloading.
pub async fn run_self_update(yes: bool, check_only: bool) -> Result<()> {
    let opts = kaishin::KaishinOptions::new(
        // owner / repo / bin-name / current-version.
        // Repo + bin happen to share the name here; the constructor
        // takes them separately so a crate whose binary differs from
        // the GitHub repo name can still self-update.
        "yukimemi",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
    );
    let upd_opts = kaishin::UpdateOptions::new()
        .yes(yes)
        .check_only(check_only)
        // shoka doesn't have a global `--non-interactive` flag yet
        // (renri's `CmdCtx` does). Default to interactive — the
        // prompt is the safety net for accidental `self-update`
        // invocations; users who want it scripted pass `-y`.
        .non_interactive(false);

    kaishin::run_self_update(&opts, upd_opts).await
}
