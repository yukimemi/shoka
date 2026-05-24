use crate::cli::CdArgs;
use crate::commands::ShokaContext;

pub async fn run(_ctx: &ShokaContext, _args: CdArgs) -> anyhow::Result<()> {
    anyhow::bail!("`shoka cd` is not implemented yet")
}
