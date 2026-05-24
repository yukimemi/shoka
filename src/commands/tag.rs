use crate::commands::ShokaContext;

pub async fn add(
    _ctx: &ShokaContext,
    _repo: Option<String>,
    _tags: Vec<String>,
) -> anyhow::Result<()> {
    anyhow::bail!("`shoka tag add` is not implemented yet")
}

pub async fn rm(
    _ctx: &ShokaContext,
    _repo: Option<String>,
    _tags: Vec<String>,
) -> anyhow::Result<()> {
    anyhow::bail!("`shoka tag rm` is not implemented yet")
}

pub async fn ls(_ctx: &ShokaContext, _repo: Option<String>) -> anyhow::Result<()> {
    anyhow::bail!("`shoka tag ls` is not implemented yet")
}

pub async fn who(_ctx: &ShokaContext, _tag: Option<String>) -> anyhow::Result<()> {
    anyhow::bail!("`shoka tag who` is not implemented yet")
}
