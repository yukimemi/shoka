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

use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;

use crate::config::AutoUpdateMode;

/// Run the self-update flow.
///
/// - `yes`: skip the "install vX.Y.Z?" confirmation prompt.
/// - `check_only`: print availability and exit without downloading.
///
/// `non_interactive` is auto-detected from `stdin().is_terminal()`:
/// when stdin isn't a TTY (cron, CI runner, piped invocation) the
/// updater is told to skip the prompt step entirely, so it errors
/// out cleanly instead of hanging on an unread `[y/N]` it can never
/// satisfy. Users running `shoka self-update` from a real terminal
/// keep getting the prompt unless they pass `-y` explicitly.
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
    let non_interactive = !std::io::stdin().is_terminal();
    let upd_opts = kaishin::UpdateOptions::new()
        .yes(yes)
        .check_only(check_only)
        .non_interactive(non_interactive);

    kaishin::run_self_update(&opts, upd_opts).await
}

/// How long `finalize_auto_update_check` waits for a background install
/// to land before giving up. Kept short so fast commands never hang on
/// a slow network — a timeout is a silent no-op, and kaishin's own
/// throttle/state file means the next invocation simply retries.
const FINALIZE_TIMEOUT: Duration = Duration::from_secs(5);

/// The env kill-switch name (`SHOKA_NO_AUTOUPDATE`).
const NO_AUTOUPDATE_ENV: &str = "SHOKA_NO_AUTOUPDATE";

/// Build the [`kaishin::KaishinOptions`] for shoka's own repo/binary.
fn kaishin_opts() -> kaishin::KaishinOptions {
    kaishin::KaishinOptions::new(
        "yukimemi",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
    )
}

/// `true` when the `SHOKA_NO_AUTOUPDATE` env kill-switch is engaged.
///
/// Disabled iff the variable is present, non-empty, and not `"0"` /
/// `"false"` (case-insensitive). This takes precedence over the config
/// `auto_update` mode — an operator can always force background
/// auto-update off without touching config files (CI, locked-down
/// environments, packaging that owns the binary, …).
pub fn auto_update_disabled_by_env() -> bool {
    match std::env::var(NO_AUTOUPDATE_ENV) {
        Ok(v) => {
            let t = v.trim();
            !t.is_empty() && !t.eq_ignore_ascii_case("0") && !t.eq_ignore_ascii_case("false")
        }
        Err(_) => false,
    }
}

/// In-flight background auto-update work, consumed by
/// [`finalize_auto_update_check`].
///
/// The variants mirror the two non-`Off` modes:
///
/// - `Notify` produces either a cached-banner short-circuit
///   ([`Self::NotifyCached`]) or a pending background fetch
///   ([`Self::NotifyPending`]).
/// - `Install` produces an in-process install task ([`Self::Installing`]).
pub enum AutoUpdateHandle {
    /// Notify mode, throttle window not yet elapsed but the cached
    /// state already shows a newer release — just print the banner.
    NotifyCached {
        checker: kaishin::Checker,
        latest: kaishin::LatestRelease,
    },
    /// Notify mode, a background `check_and_save` is in flight.
    NotifyPending {
        checker: kaishin::Checker,
        handle: tokio::task::JoinHandle<Result<Option<kaishin::LatestRelease>>>,
        /// Fallback for the timeout / error case.
        cached_latest: Option<kaishin::LatestRelease>,
    },
    /// Install mode — an in-process `auto_update()` task is downloading +
    /// swapping the binary. `Ok(Some(latest))` means an install
    /// actually happened.
    Installing {
        handle: tokio::task::JoinHandle<Result<Option<kaishin::LatestRelease>>>,
    },
}

/// Spawn the background auto-update work for `mode`, if any.
///
/// Returns `None` (nothing to finalize) when `mode` is
/// [`AutoUpdateMode::Off`], or — for `Notify` — when the throttle
/// window hasn't elapsed and there's no cached newer release. All
/// failures upstream of this (config load, env) are handled by the
/// caller; this function itself never panics.
///
/// `state_path` is the JSON throttle file (kaishin persists the
/// last-check timestamp + last-known release there); it should live
/// under shoka's cache dir so `SHOKA_CACHE_DIR` is honoured.
pub fn maybe_spawn_auto_update_check(
    mode: AutoUpdateMode,
    interval: Duration,
    state_path: PathBuf,
) -> Option<AutoUpdateHandle> {
    // Match on `mode` directly and only build the `checker` in the arms
    // that need it — `Off` short-circuits with no allocation or state I/O.
    match mode {
        AutoUpdateMode::Off => None,
        AutoUpdateMode::Notify => {
            let checker = kaishin::Checker::new(env!("CARGO_PKG_NAME"), kaishin_opts())
                .interval(interval)
                .state_path(state_path);
            // Notify never installs: it only fetches + banners.
            if !checker.should_check() {
                // Throttle window — fall back to the cached state.
                return checker
                    .cached_update()
                    .map(|latest| AutoUpdateHandle::NotifyCached { checker, latest });
            }
            let cached_latest = checker.cached_update();
            let checker_clone = checker.clone();
            let handle = tokio::spawn(async move { checker_clone.check_and_save().await });
            Some(AutoUpdateHandle::NotifyPending {
                checker,
                handle,
                cached_latest,
            })
        }
        AutoUpdateMode::Install => {
            let checker = kaishin::Checker::new(env!("CARGO_PKG_NAME"), kaishin_opts())
                .interval(interval)
                .state_path(state_path);
            // Silent install. `auto_update()` is self-throttled
            // (it returns Ok(None) immediately inside the window), so
            // it's safe to spawn unconditionally — no should_check()
            // gate needed here, and skipping it avoids a redundant
            // state-file read on the hot path.
            let handle = tokio::spawn(async move { checker.auto_update().await });
            Some(AutoUpdateHandle::Installing { handle })
        }
    }
}

/// Consume an [`AutoUpdateHandle`], printing at most one stderr line.
///
/// - `Notify*` → print the kaishin banner when a newer release is
///   known (cached or freshly fetched).
/// - `Installing` → wait up to [`FINALIZE_TIMEOUT`]; if an install
///   actually landed, print exactly one line:
///   `✓ shoka <version> installed in the background — restart to apply.`
///
/// Timeouts, "already up to date", and any error all print nothing —
/// background auto-update is silent by design.
pub async fn finalize_auto_update_check(handle: AutoUpdateHandle) {
    match handle {
        AutoUpdateHandle::NotifyCached { checker, latest } => {
            eprintln!("\n{}", checker.format_banner(&latest));
        }
        AutoUpdateHandle::NotifyPending {
            checker,
            handle,
            cached_latest,
        } => {
            match tokio::time::timeout(FINALIZE_TIMEOUT, handle).await {
                Ok(Ok(Ok(Some(latest)))) => {
                    eprintln!("\n{}", checker.format_banner(&latest));
                }
                Ok(Ok(Ok(None))) => {
                    // Fetched successfully, nothing newer — stay quiet.
                }
                _ => {
                    // Timeout / join error / fetch error: fall back to
                    // the cached state if it shows a newer release.
                    if let Some(latest) = cached_latest {
                        eprintln!("\n{}", checker.format_banner(&latest));
                    }
                }
            }
        }
        AutoUpdateHandle::Installing { handle } => {
            // Only announce a real install that completed inside the
            // window. Everything else (timeout, no update, error) is
            // silent. The install runs in-process, so if it is still
            // downloading when the process exits it is aborted (kaishin
            // swaps atomically, so the binary is never left torn) and a
            // later invocation retries within the next throttle window.
            if let Ok(Ok(Ok(Some(latest)))) = tokio::time::timeout(FINALIZE_TIMEOUT, handle).await {
                let version = latest.tag_name.trim_start_matches('v');
                eprintln!(
                    "\u{2713} {bin} {version} installed in the background — restart to apply.",
                    bin = env!("CARGO_PKG_NAME"),
                );
            }
        }
    }
}
