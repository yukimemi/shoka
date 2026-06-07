//! `shoka cache` — per-repo volatile cache management.
//!
//! Phase-1 implementation: synchronous refresh that walks the shelf,
//! honours the `[global.cache].refresh_threshold_secs` TTL, and
//! updates the `last_refreshed` timestamp on stale entries. Actual
//! git-status / gh-PR snapshot collection is a TODO — this PR lays
//! the cache plumbing so the data-gathering PRs can drop in without
//! restructuring callers.
//!
//! Two refresh modes:
//!
//! - **Foreground** (`shoka cache refresh [--force] [--tag ...]`) —
//!   the user-facing path: pretty summary output, errors bubble up.
//! - **Background** (`shoka cache refresh --background`, hidden
//!   flag) — used by [`try_spawn_bg_refresh`] when the dispatcher
//!   spawns a detached subprocess at the tail of other commands.
//!   Output is suppressed; errors are downgraded to `tracing::warn`
//!   so the detached child never disturbs the parent's terminal.
//!
//! [`Cache`]: crate::cache::Cache
//! [`Shelf`]: crate::state::Shelf

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use owo_colors::OwoColorize;

use crate::cache::{Cache, current_unix_secs};
use crate::cli::CacheCommand;
use crate::commands::ShokaContext;
use crate::config::ShokaConfig;
use crate::gh;
use crate::git_status;
use crate::state::{Repo, Shelf};
use teravars::Engine;

pub async fn dispatch(ctx: &ShokaContext, cmd: CacheCommand) -> Result<()> {
    match cmd {
        CacheCommand::Refresh {
            force,
            tags,
            background,
        } => {
            if background {
                // Best-effort: log and swallow. The detached child
                // must never propagate failures since stderr is
                // /dev/null'd anyway and there's no parent to
                // observe the exit code.
                if let Err(e) = refresh(ctx, force, tags, /*background=*/ true).await {
                    tracing::warn!(target: "shoka", "background refresh failed: {e:#}");
                }
                Ok(())
            } else {
                refresh(ctx, force, tags, /*background=*/ false).await
            }
        }
        CacheCommand::Show => show(ctx).await,
        CacheCommand::Clear => clear(ctx).await,
    }
}

async fn refresh(
    ctx: &ShokaContext,
    force: bool,
    tags: Vec<String>,
    background: bool,
) -> Result<()> {
    let cfg = ShokaConfig::load(&ctx.paths)?;
    let resolved = cfg.resolve(ctx.profile_override.as_deref())?;
    let shelf = Shelf::load(&ctx.paths)?;
    let mut cache = Cache::load(&ctx.paths)?;

    let now = current_unix_secs();
    let threshold = resolved.cache.refresh_threshold_secs;

    let mut refreshed = 0;
    let mut skipped_threshold = 0;
    let mut skipped_filter = 0;
    let mut capture_errors = 0;
    let mut gh_errors = 0;

    // One Tera engine for the whole walk — clone_path_for renders
    // per repo, and the engine setup cost dominates a sub-ms gix
    // status call on a small repo.
    let mut engine = Engine::new();

    // Build a gh client once if a token is reachable. Missing token
    // just means gh fields stay None — non-gh capture (gix status)
    // is unaffected. Token resolution is cheap, but the client
    // includes an HTTP connection pool we want to share across
    // repos; pulling it inside the loop would be silly.
    let gh_client = match gh::resolve_token().await {
        Some(token) => match gh::build_client(&token) {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!(target: "shoka", "gh client init failed: {e:#}");
                None
            }
        },
        None => None,
    };

    for repo in &shelf.repos {
        if !tags.is_empty() && !has_all_tags(repo, &tags) {
            skipped_filter += 1;
            continue;
        }
        // Resolve the clone path *before* mutably borrowing the
        // cache entry: resolved.clone_path_for can fail, and bailing
        // mid-mutation would leave the cache half-updated.
        let path = match resolved.clone_path_for(repo, &mut engine) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    target: "shoka",
                    "skipping {} for refresh: path resolve failed ({e:#})",
                    repo.slug()
                );
                capture_errors += 1;
                continue;
            }
        };

        let entry = cache.upsert(repo);
        if !force && !entry.is_stale(threshold, now) {
            skipped_threshold += 1;
            continue;
        }

        // Capture the gix snapshot. Errors are logged + counted but
        // don't abort the refresh — one broken repo shouldn't stop
        // the rest of the shelf from updating. The previous snapshot
        // (if any) survives so the TUI keeps showing the last-known
        // state rather than going blank.
        match git_status::capture(&path) {
            Ok(snapshot) => {
                entry.git_status = Some(snapshot);
            }
            Err(e) => {
                tracing::warn!(
                    target: "shoka",
                    "git status capture failed for {} at {}: {e:#}",
                    repo.slug(),
                    path.display()
                );
                capture_errors += 1;
            }
        }

        // gh snapshot: only attempt when a client is available AND
        // the host is github.com (other hosts would return 404).
        // Per-call errors are logged but never propagated — a rate-
        // limited or temporarily-unreachable API shouldn't blank
        // the dashboard. Phase 2b: serial, because parallelising
        // the loop while keeping `cache` mutably borrowed is more
        // restructuring than this PR can absorb; the parallel-fanout
        // PR can land separately once we collect the rest of the
        // refresh into a tasks-then-merge shape.
        if let Some(client) = gh_client.as_ref() {
            if repo.host == "github.com" {
                match gh::capture_snapshot(client, &repo.owner, &repo.name).await {
                    Ok(snap) => entry.gh = Some(snap),
                    Err(e) => {
                        tracing::warn!(
                            target: "shoka",
                            "gh snapshot failed for {}: {e:#}",
                            repo.slug()
                        );
                        gh_errors += 1;
                    }
                }
            }
        }

        entry.last_refreshed = Some(now);
        refreshed += 1;
    }

    cache.save(&ctx.paths)?;

    if background {
        // tracing-only: stdout/stderr are /dev/null'd in the
        // detached child anyway, but tracing-subscriber respects
        // SHOKA_LOG for users who want to peek at refresh history.
        tracing::info!(
            target: "shoka",
            "background cache refresh: {refreshed} updated, {skipped_threshold} fresh, {skipped_filter} filtered, {capture_errors} capture errors, {gh_errors} gh errors ({} on shelf)",
            shelf.len()
        );
    } else {
        println!(
            "{} refreshed {}{} repo{} ({} on the shelf)",
            "cache:".bold(),
            refreshed,
            if force { " (forced)" } else { "" },
            if refreshed == 1 { "" } else { "s" },
            shelf.len()
        );
        if skipped_threshold > 0 {
            println!(
                "  {} {} fresh (within {}s threshold)",
                "↩".dimmed(),
                skipped_threshold,
                threshold
            );
        }
        if skipped_filter > 0 {
            println!(
                "  {} {} filtered out by --tag",
                "↩".dimmed(),
                skipped_filter
            );
        }
        if capture_errors > 0 {
            println!(
                "  {} {} capture errors (previous snapshot kept; see SHOKA_LOG=warn)",
                "!".red(),
                capture_errors
            );
        }
        if gh_errors > 0 {
            println!(
                "  {} {} gh API errors (rate limit / 404? previous snapshot kept)",
                "!".red(),
                gh_errors
            );
        }
        if gh_client.is_none() {
            println!(
                "  {} no GITHUB_TOKEN / gh CLI — PR + CI columns will stay empty",
                "↩".dimmed()
            );
        }
    }
    Ok(())
}

async fn show(ctx: &ShokaContext) -> Result<()> {
    let cache = Cache::load(&ctx.paths)?;
    let body = toml::to_string_pretty(&cache)?;
    print!("{body}");
    Ok(())
}

async fn clear(ctx: &ShokaContext) -> Result<()> {
    let cache = Cache::default();
    cache.save(&ctx.paths)?;
    println!("{} cleared", "cache:".bold());
    Ok(())
}

fn has_all_tags(repo: &Repo, wanted: &[String]) -> bool {
    wanted.iter().all(|w| repo.tags.iter().any(|t| t == w))
}

/// Attempt to spawn a detached background `shoka cache refresh
/// --background` subprocess.
///
/// Best-effort by design — the bg refresh is opportunistic, so
/// callers should *log* but never *bail* on errors. The child runs
/// with stdin / stdout / stderr null'd and is detached from the
/// parent process group so it survives the parent's exit:
///
/// - Unix: `setsid(2)` in `pre_exec` so the child becomes its own
///   session leader (no controlling terminal, immune to SIGHUP when
///   the parent shell exits).
/// - Windows: `DETACHED_PROCESS | CREATE_NO_WINDOW |
///   CREATE_NEW_PROCESS_GROUP`. See [`bg_refresh_creation_flags`]
///   for the rationale of each flag — short version is
///   `DETACHED_PROCESS` decouples from the parent's console
///   lifetime (so closing the terminal doesn't kill the child),
///   `CREATE_NO_WINDOW` skips the new-console window allocation
///   that Windows otherwise does for a console-subsystem exe (no
///   black flash at the tail of every shoka command), and
///   `CREATE_NEW_PROCESS_GROUP` gives the child its own group so
///   it ignores Ctrl-C sent to the parent shell.
///
/// `SHOKA_CONFIG` and `SHOKA_PROFILE` are propagated via env so the
/// child sees the same config file and active profile the parent
/// used (otherwise the child would load defaults and write a
/// different cache).
///
/// Gated by `[global.cache].background_refresh`. When that's
/// `false`, this function is an opt-out: it loads enough config to
/// see the flag, then returns `Ok(())` without spawning anything.
pub fn try_spawn_bg_refresh(ctx: &ShokaContext) -> Result<()> {
    let cfg = ShokaConfig::load(&ctx.paths)?;
    let resolved = cfg.resolve(ctx.profile_override.as_deref())?;
    if !resolved.cache.background_refresh {
        tracing::debug!(target: "shoka", "background refresh disabled by config");
        return Ok(());
    }
    let exe = std::env::current_exe().context("locating current shoka executable")?;
    spawn_detached(
        &exe,
        ctx.paths.config_file(),
        ctx.profile_override.as_deref(),
    )
    .context("spawning background refresh")?;
    Ok(())
}

fn spawn_detached(exe: &Path, config_file: &Path, profile: Option<&str>) -> Result<()> {
    let mut cmd = Command::new(exe);
    cmd.args(["cache", "refresh", "--background"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env("SHOKA_CONFIG", config_file);
    if let Some(p) = profile {
        cmd.env("SHOKA_PROFILE", p);
    }

    // OS-specific detach. On both platforms `spawn()` returns
    // immediately without waiting; the child outlives the parent.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: `setsid()` from libc returns the new session id
        // or -1 on error; we don't check the return because failure
        // (already a session leader, etc.) is harmless for our
        // "best-effort detach" goal. The closure is the standard
        // pre_exec idiom — it runs after fork(2) but before exec(3)
        // in the child only.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(bg_refresh_creation_flags());
    }

    cmd.spawn()
        .with_context(|| format!("spawning {} cache refresh --background", exe.display()))?;
    Ok(())
}

/// Windows `CreateProcess` creation flags for the detached background
/// refresh subprocess. Pulled out of `spawn_detached` so the
/// composition can be unit-tested without spawning anything.
///
/// All three flags are necessary; dropping any one breaks behaviour:
///
/// - `DETACHED_PROCESS` — child doesn't inherit the parent's console
///   and is decoupled from the parent's console lifetime. Without
///   this, closing the terminal window (or the parent shell exiting)
///   sends a console-close signal that kills the child too.
/// - `CREATE_NO_WINDOW` — suppresses the *new* console window that
///   Windows would otherwise allocate for the child (shoka is a
///   console-subsystem binary, so it'd get its own console even
///   when detached). Without this, every shoka command tail flashes
///   a black window for an instant.
/// - `CREATE_NEW_PROCESS_GROUP` — child has its own process group,
///   immune to Ctrl-C sent to the parent shell's group.
#[cfg(windows)]
pub(crate) const fn bg_refresh_creation_flags() -> u32 {
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::Cache;
    use crate::state::{Repo, Shelf};
    use std::path::Path;
    use tempfile::TempDir;

    fn paths_at(tmp: &Path) -> crate::paths::ShokaPaths {
        crate::paths::ShokaPaths::rooted_at(tmp)
    }

    fn sample(name: &str) -> Repo {
        Repo::new("github.com", "yukimemi", name)
    }

    #[test]
    fn paths_at_never_points_at_the_real_cache() {
        // Regression guard for the 2026-06-07 incident: the old
        // helper pinned only the *config* path via
        // `resolve(Some(...))`, leaving state/cache on the OS
        // defaults — so `clear_empties_cache_file_atomically`'s
        // `Cache::default().save()` wiped the developer's real
        // cache.toml on every `cargo test` run.
        let tmp = TempDir::new().unwrap();
        let paths = paths_at(tmp.path());
        assert!(
            paths.cache_file().starts_with(tmp.path()),
            "cache must stay under the tempdir, got {:?}",
            paths.cache_file()
        );
        assert!(
            paths.state_file().starts_with(tmp.path()),
            "state must stay under the tempdir, got {:?}",
            paths.state_file()
        );
    }

    #[test]
    fn refresh_walks_shelf_and_marks_entries() {
        let mut shelf = Shelf::default();
        shelf.add(sample("shoka")).unwrap();
        shelf.add(sample("renri")).unwrap();
        let mut cache = Cache::default();
        let now = 1_700_000_000;
        let threshold = 60;

        for repo in &shelf.repos {
            let entry = cache.upsert(repo);
            if entry.is_stale(threshold, now) {
                entry.last_refreshed = Some(now);
            }
        }
        assert_eq!(cache.len(), 2);
        assert_eq!(
            cache
                .find("github.com", "yukimemi", "shoka", None)
                .unwrap()
                .last_refreshed,
            Some(now)
        );
    }

    #[test]
    fn refresh_respects_threshold_without_force() {
        let mut cache = Cache::default();
        let repo = sample("shoka");
        cache.upsert(&repo).last_refreshed = Some(1_700_000_000);

        let entry = cache
            .find_mut("github.com", "yukimemi", "shoka", None)
            .unwrap();
        assert!(!entry.is_stale(60, 1_700_000_010));
        let entry = cache
            .find_mut("github.com", "yukimemi", "shoka", None)
            .unwrap();
        assert!(entry.is_stale(60, 1_700_000_120));
    }

    #[test]
    fn clear_empties_cache_file_atomically() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_at(tmp.path());
        let mut cache = Cache::default();
        cache.upsert(&sample("shoka")).last_refreshed = Some(1);
        cache.save(&paths).unwrap();
        assert!(paths.cache_file().exists());

        Cache::default().save(&paths).unwrap();
        let loaded = Cache::load(&paths).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn has_all_tags_and_semantics() {
        let mut repo = sample("shoka");
        repo.tags = vec!["rust".into(), "cli".into()];
        assert!(has_all_tags(&repo, &["rust".into()]));
        assert!(has_all_tags(&repo, &["rust".into(), "cli".into()]));
        assert!(!has_all_tags(
            &repo,
            &["rust".into(), "cli".into(), "tui".into()]
        ));
        assert!(has_all_tags(&repo, &[]));
    }

    #[test]
    fn try_spawn_bg_refresh_short_circuits_when_disabled() {
        // Stage a config that opts out of background refresh and
        // confirm `try_spawn_bg_refresh` returns Ok without
        // touching the filesystem outside the temp tree.
        use std::fs;
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("config.toml"),
            r#"
[global]
root = "/r"

[global.cache]
background_refresh = false
"#,
        )
        .unwrap();
        let paths = paths_at(tmp.path());
        let ctx = ShokaContext {
            paths,
            profile_override: None,
        };
        // Just asserts the function returns Ok in the opt-out case.
        // Actually spawning is a side effect we don't want to
        // exercise under `cargo test`, so we don't construct the
        // "enabled" counterpart here.
        try_spawn_bg_refresh(&ctx).expect("opt-out path returns Ok");
    }

    #[cfg(windows)]
    #[test]
    fn bg_refresh_creation_flags_compose_all_three() {
        // Regression for the Windows console-flash fix: each of the
        // three Win32 creation flags carries distinct, non-overlapping
        // bits; dropping any one re-introduces a visible bug (terminal-
        // close kills the child, black-flash, or Ctrl-C kills the child
        // with the parent). Assert the constant directly so a future
        // edit that "simplifies" the composition trips this test.
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let flags = bg_refresh_creation_flags();
        assert_eq!(flags & DETACHED_PROCESS, DETACHED_PROCESS);
        assert_eq!(flags & CREATE_NEW_PROCESS_GROUP, CREATE_NEW_PROCESS_GROUP);
        assert_eq!(flags & CREATE_NO_WINDOW, CREATE_NO_WINDOW);
        // No stray bits — keep the surface minimal so a future
        // contributor doesn't inherit a flag they didn't intend.
        assert_eq!(
            flags,
            DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW
        );
    }
}
