use crate::cli::CloneArgs;
use crate::commands::ShokaContext;

pub async fn run(_ctx: &ShokaContext, _args: CloneArgs) -> anyhow::Result<()> {
    anyhow::bail!("`shoka clone` is not implemented yet")
}
