use crate::cli::NoteArgs;
use crate::commands::ShokaContext;

pub async fn run(_ctx: &ShokaContext, _args: NoteArgs) -> anyhow::Result<()> {
    anyhow::bail!("`shoka note` is not implemented yet")
}
