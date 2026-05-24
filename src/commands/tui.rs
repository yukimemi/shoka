use crate::cli::TuiArgs;
use crate::commands::ShokaContext;

pub async fn run(_ctx: &ShokaContext, _args: TuiArgs) -> anyhow::Result<()> {
    anyhow::bail!("`shoka tui` is not implemented yet (phase 2 deliverable)")
}
