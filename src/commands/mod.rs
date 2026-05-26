use crate::cli::{Cli, Command, TagCommand, TuiArgs};
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
pub mod note;
pub mod prune;
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

    let result = match cmd {
        Command::Clone(a) => clone::run(&ctx, a).await,
        Command::List(a) => list::run(&ctx, a).await,
        Command::Cd(a) => cd::run(&ctx, a).await,
        Command::Exec(a) => exec::run(&ctx, a).await,
        Command::Prune(a) => prune::run(&ctx, a).await,
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

    result
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
}
