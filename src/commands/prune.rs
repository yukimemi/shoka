use crate::cli::PruneArgs;
use crate::commands::ShokaContext;

pub async fn run(_ctx: &ShokaContext, _args: PruneArgs) -> anyhow::Result<()> {
    anyhow::bail!("`shoka prune` is not implemented yet")
}
