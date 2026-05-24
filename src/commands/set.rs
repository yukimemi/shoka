use crate::cli::SetArgs;
use crate::commands::ShokaContext;

pub async fn run(_ctx: &ShokaContext, _args: SetArgs) -> anyhow::Result<()> {
    anyhow::bail!("`shoka set` is not implemented yet")
}
