use crate::cli::ExecArgs;
use crate::commands::ShokaContext;

pub async fn run(_ctx: &ShokaContext, _args: ExecArgs) -> anyhow::Result<()> {
    anyhow::bail!("`shoka exec` is not implemented yet")
}
