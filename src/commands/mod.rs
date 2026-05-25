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
    match cmd {
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
    }
}
