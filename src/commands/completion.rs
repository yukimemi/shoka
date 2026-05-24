use crate::cli::{Cli, CompletionArgs};
use clap::CommandFactory;

pub async fn run(args: CompletionArgs) -> anyhow::Result<()> {
    let mut cmd = Cli::command();
    let bin = cmd.get_name().to_string();
    clap_complete::generate(args.shell, &mut cmd, bin, &mut std::io::stdout());
    Ok(())
}
