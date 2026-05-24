use crate::cli::ImportArgs;
use crate::commands::ShokaContext;

pub async fn run(_ctx: &ShokaContext, _args: ImportArgs) -> anyhow::Result<()> {
    anyhow::bail!("`shoka import` is not implemented yet")
}
