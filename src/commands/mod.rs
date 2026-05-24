use crate::cli::{Cli, Command, TagCommand, TuiArgs};

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

pub async fn dispatch(cli: Cli) -> anyhow::Result<()> {
    let cmd = cli
        .cmd
        .unwrap_or(Command::Tui(TuiArgs { tags: Vec::new() }));
    match cmd {
        Command::Clone(a) => clone::run(a).await,
        Command::List(a) => list::run(a).await,
        Command::Cd(a) => cd::run(a).await,
        Command::Exec(a) => exec::run(a).await,
        Command::Prune(a) => prune::run(a).await,
        Command::Import(a) => import::run(a).await,
        Command::Export(a) => export::run(a).await,
        Command::Tag(t) => match t {
            TagCommand::Add { repo, tags } => tag::add(repo, tags).await,
            TagCommand::Rm { repo, tags } => tag::rm(repo, tags).await,
            TagCommand::Ls { repo } => tag::ls(repo).await,
            TagCommand::Who { tag } => tag::who(tag).await,
        },
        Command::Set(a) => set::run(a).await,
        Command::Note(a) => note::run(a).await,
        Command::Doctor => doctor::run().await,
        Command::Tui(a) => tui::run(a).await,
        Command::Completion(a) => completion::run(a).await,
        Command::InitShell(a) => init_shell::run(a).await,
    }
}
