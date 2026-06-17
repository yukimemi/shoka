use crate::cli::{Cli, Command, TagCommand, TuiArgs};
use crate::config::{AutoUpdateMode, ShokaConfig};
use crate::paths::ShokaPaths;

pub mod cache;
pub mod cd;
pub mod clone;
pub mod completion;
pub mod doctor;
pub mod exec;
pub mod export;
pub mod import;
pub mod init_shell;
pub mod list;
pub mod new;
pub mod note;
pub mod prune;
pub mod rm;
pub mod set;
pub mod tag;
pub mod tui;

/// Per-invocation context shared with every subcommand.
///
/// Built once in [`dispatch`] from the global CLI flags and threaded
/// through. Lazy on purpose: [`ShokaConfig`] isn't loaded here so that
/// commands which don't need config (e.g. `completion`, `init-shell`)
/// don't pay for it.
///
/// [`ShokaConfig`]: crate::config::ShokaConfig
#[derive(Debug, Clone)]
pub struct ShokaContext {
    pub paths: ShokaPaths,
    /// Profile name from `--profile` / `$SHOKA_PROFILE`. Subcommands
    /// pass this through to [`crate::config::ShokaConfig::resolve`].
    pub profile_override: Option<String>,
}

pub async fn dispatch(cli: Cli) -> anyhow::Result<()> {
    let ctx = ShokaContext {
        paths: ShokaPaths::resolve(cli.config.as_deref())?,
        profile_override: cli.profile,
    };
    let cmd = cli
        .cmd
        .unwrap_or(Command::Tui(TuiArgs { tags: Vec::new() }));

    // Decide BEFORE running the subcommand, so a panicking or
    // erroring subcommand still gets the bg refresh fired. The bg
    // refresh is opportunistic — its job is "freshen the cache for
    // the next command", and that holds even when this one failed.
    let bg_eligible = bg_refresh_eligible(&cmd);

    // Spawn the background auto-update check before running the
    // subcommand so the GitHub round-trip / download overlaps the
    // command's own work. The handle is finalized after the command
    // (and the bg-refresh spawn) below. Skipped entirely for the
    // commands in `auto_update_eligible`'s exclusion list. Everything
    // here is best-effort: a failure to read config, an env kill, or
    // mode == Off all collapse to `None` and print nothing.
    let auto_update_handle = if auto_update_eligible(&cmd) {
        maybe_spawn_auto_update(&ctx)
    } else {
        None
    };

    let result = match cmd {
        Command::Clone(a) => clone::run(&ctx, a).await,
        Command::New(a) => new::run(&ctx, a).await,
        Command::List(a) => list::run(&ctx, a).await,
        Command::Cd(a) => cd::run(&ctx, a).await,
        Command::Exec(a) => exec::run(&ctx, a).await,
        Command::Prune(a) => prune::run(&ctx, a).await,
        Command::Rm(a) => rm::run(&ctx, a).await,
        Command::Import(a) => import::run(&ctx, a).await,
        Command::Export(a) => export::run(&ctx, a).await,
        Command::Tag(t) => match t {
            TagCommand::Add { repo, tags } => tag::add(&ctx, repo, tags).await,
            TagCommand::Rm { repo, tags } => tag::rm(&ctx, repo, tags).await,
            TagCommand::Ls { repo } => tag::ls(&ctx, repo).await,
            TagCommand::Who { tag } => tag::who(&ctx, tag).await,
        },
        Command::Set(a) => set::run(&ctx, a).await,
        Command::Note(a) => note::run(&ctx, a).await,
        Command::Doctor => doctor::run(&ctx).await,
        Command::Tui(a) => tui::run(&ctx, a).await,
        Command::Completion(a) => completion::run(a).await,
        Command::InitShell(a) => init_shell::run(a).await,
        Command::Cache(c) => cache::dispatch(&ctx, c).await,
        Command::SelfUpdate(a) => crate::updater::run_self_update(a.yes, a.check).await,
    };

    if bg_eligible {
        // Best-effort: never let a failed bg-refresh spawn affect
        // the user-visible exit. The `try_spawn_bg_refresh` helper
        // itself silently returns Ok when the config opts out.
        if let Err(e) = cache::try_spawn_bg_refresh(&ctx) {
            tracing::debug!(target: "shoka", "background refresh spawn failed: {e:#}");
        }
    }

    // Finalize the background auto-update last, with a short bounded
    // wait (5s) so a slow network can't make a fast command hang. A
    // timeout prints nothing; the install runs in-process, so if it is
    // still downloading when the process exits it is aborted (kaishin
    // swaps atomically) and a later invocation retries within the next
    // throttle window.
    if let Some(handle) = auto_update_handle {
        crate::updater::finalize_auto_update_check(handle).await;
    }

    result
}

/// Resolve the background auto-update handle for this invocation, or
/// `None` to do nothing.
///
/// Returns `None` when:
/// - the `SHOKA_NO_AUTOUPDATE` env kill-switch is engaged (takes
///   precedence over config), or
/// - config can't be loaded / resolved (best-effort — never surface a
///   config error through the auto-update path), or
/// - the resolved mode is [`AutoUpdateMode::Off`], or
/// - `Notify` mode is inside its throttle window with no cached update.
///
/// The throttle state file lives under the cache dir so
/// `SHOKA_CACHE_DIR` is honoured, alongside the existing cache.
fn maybe_spawn_auto_update(ctx: &ShokaContext) -> Option<crate::updater::AutoUpdateHandle> {
    // Env kill-switch wins over everything, including a Notify/Install
    // config — and short-circuits before any config I/O.
    if crate::updater::auto_update_disabled_by_env() {
        return None;
    }

    // Best-effort config load: a missing/broken config must not break
    // auto-update (it just disables it for this run).
    let resolved = ShokaConfig::load(&ctx.paths)
        .ok()?
        .resolve(ctx.profile_override.as_deref())
        .ok()?;

    if matches!(resolved.auto_update, AutoUpdateMode::Off) {
        return None;
    }

    let interval = resolved
        .update_check_interval
        .as_deref()
        .and_then(|s| kaishin::parse_interval(s).ok())
        .unwrap_or_else(kaishin::default_interval);

    let state_path = ctx.paths.cache_dir().join("last_update_check.json");
    crate::updater::maybe_spawn_auto_update_check(resolved.auto_update, interval, state_path)
}

/// Decide whether the dispatcher should fire a background cache
/// refresh after the subcommand finishes.
///
/// Excludes:
///
/// - `cache` itself — would loop forever; the explicit `cache
///   refresh` already updates the cache.
/// - `completion` / `init-shell` — script generators that don't
///   touch the shelf or read the cache. Spawning a refresh from
///   them surprises shells doing tab-completion lookups.
/// - `tui` — long-running; the TUI owns its own refresh strategy
///   (will land with the dashboard PR). Firing a one-off refresh
///   at start would compete with that.
/// - `self-update` — the binary just got swapped under us. Spawning
///   `<old-path> cache refresh --background` against the now-
///   replaced executable is at best wasted work and at worst hits
///   "text file busy" or weird OS-loader edge cases on Windows.
///
/// Doctor is *not* excluded: it doesn't write state but it's a
/// natural "are things OK" checkpoint, and freshening the cache
/// after a doctor run benefits the next subcommand.
fn bg_refresh_eligible(cmd: &Command) -> bool {
    !matches!(
        cmd,
        Command::Cache(_)
            | Command::Completion(_)
            | Command::InitShell(_)
            | Command::Tui(_)
            | Command::SelfUpdate(_)
    )
}

/// Decide whether the dispatcher should run a background auto-update
/// check for this subcommand.
///
/// Excludes:
///
/// - `self-update` — the user is already running the explicit,
///   interactive update flow; a background install racing it makes no
///   sense.
/// - `completion` / `init-shell` — script generators whose stdout is
///   consumed verbatim by the shell. Even though the auto-update notice
///   goes to stderr, these are fast and pointless to gate an update on;
///   keeping them out avoids any chance of noise around shell
///   integration.
/// - `tui` — long-running and owns the alternate screen. A banner (or
///   the 5s finalize wait) racing the TUI teardown would corrupt the
///   display, so skip it; the next non-TUI command will run the check.
///
/// In particular `cd` is *not* excluded: it's fast, but it's also one
/// of the most frequently run commands, so it's a good carrier for the
/// throttled check. Its PATH output goes to stdout while the
/// auto-update notice goes only to stderr, so the shell wrapper is
/// never corrupted.
fn auto_update_eligible(cmd: &Command) -> bool {
    !matches!(
        cmd,
        Command::SelfUpdate(_) | Command::Completion(_) | Command::InitShell(_) | Command::Tui(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{
        CacheCommand, CdArgs, CloneArgs, CompletionArgs, InitShellArgs, ListArgs, SelfUpdateArgs,
        SupportedShell, TuiArgs,
    };

    fn cache_refresh() -> Command {
        Command::Cache(CacheCommand::Refresh {
            force: false,
            tags: vec![],
            background: false,
        })
    }

    #[test]
    fn bg_refresh_eligible_excludes_cache_and_friends() {
        assert!(!bg_refresh_eligible(&cache_refresh()));
        assert!(!bg_refresh_eligible(&Command::Cache(CacheCommand::Show)));
        assert!(!bg_refresh_eligible(&Command::Cache(CacheCommand::Clear)));
        assert!(!bg_refresh_eligible(&Command::Completion(CompletionArgs {
            shell: clap_complete::Shell::Bash
        })));
        assert!(!bg_refresh_eligible(&Command::InitShell(InitShellArgs {
            shell: SupportedShell::Bash,
            name: "s".into(),
        })));
        assert!(!bg_refresh_eligible(&Command::Tui(TuiArgs {
            tags: vec![],
        })));
        // self-update swaps the binary; spawning a refresh from the
        // now-replaced executable is wasted work at best and a
        // platform-dependent gotcha at worst.
        assert!(!bg_refresh_eligible(&Command::SelfUpdate(SelfUpdateArgs {
            yes: false,
            check: false,
        })));
    }

    #[test]
    fn bg_refresh_eligible_includes_user_facing_commands() {
        assert!(bg_refresh_eligible(&Command::Clone(CloneArgs {
            url: None
        })));
        assert!(bg_refresh_eligible(&Command::List(ListArgs {
            tags: vec![],
            has_agents: false,
        })));
        assert!(bg_refresh_eligible(&Command::Cd(CdArgs {
            repo: None,
            tags: vec![],
        })));
        assert!(bg_refresh_eligible(&Command::Doctor));
    }

    #[test]
    fn auto_update_eligible_excludes_self_update_and_script_generators() {
        // self-update is the explicit interactive flow — a background
        // install racing it makes no sense.
        assert!(!auto_update_eligible(&Command::SelfUpdate(
            SelfUpdateArgs {
                yes: false,
                check: false,
            }
        )));
        // Script generators: keep stdout clean for the shell.
        assert!(!auto_update_eligible(&Command::Completion(
            CompletionArgs {
                shell: clap_complete::Shell::Bash
            }
        )));
        assert!(!auto_update_eligible(&Command::InitShell(InitShellArgs {
            shell: SupportedShell::Bash,
            name: "s".into(),
        })));
        // TUI owns the alt-screen; a banner / finalize wait racing its
        // teardown would corrupt the display.
        assert!(!auto_update_eligible(&Command::Tui(TuiArgs {
            tags: vec![],
        })));
    }

    #[test]
    fn auto_update_eligible_includes_fast_and_long_commands() {
        // `cd` is fast but a fine throttled carrier; its PATH goes to
        // stdout while the notice goes to stderr, so no corruption.
        assert!(auto_update_eligible(&Command::Cd(CdArgs {
            repo: None,
            tags: vec![],
        })));
        // clone / list / exec are the slow commands the 5s finalize
        // cap is really protecting.
        assert!(auto_update_eligible(&Command::Clone(CloneArgs {
            url: None
        })));
        assert!(auto_update_eligible(&Command::List(ListArgs {
            tags: vec![],
            has_agents: false,
        })));
        // cache refresh is excluded from bg-refresh but not from
        // auto-update — they're independent concerns.
        assert!(auto_update_eligible(&cache_refresh()));
        assert!(auto_update_eligible(&Command::Doctor));
    }
}
