use crate::cli::ListArgs;
use crate::commands::ShokaContext;

pub async fn run(_ctx: &ShokaContext, _args: ListArgs) -> anyhow::Result<()> {
    anyhow::bail!("`shoka list` is not implemented yet")
}
