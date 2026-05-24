use crate::cli::PruneArgs;

pub async fn run(_args: PruneArgs) -> anyhow::Result<()> {
    anyhow::bail!("`shoka prune` is not implemented yet")
}
