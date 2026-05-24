use crate::cli::ExportArgs;
use crate::commands::ShokaContext;

pub async fn run(_ctx: &ShokaContext, _args: ExportArgs) -> anyhow::Result<()> {
    anyhow::bail!("`shoka export` is not implemented yet")
}
