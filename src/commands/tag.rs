pub async fn add(_repo: Option<String>, _tags: Vec<String>) -> anyhow::Result<()> {
    anyhow::bail!("`shoka tag add` is not implemented yet")
}

pub async fn rm(_repo: Option<String>, _tags: Vec<String>) -> anyhow::Result<()> {
    anyhow::bail!("`shoka tag rm` is not implemented yet")
}

pub async fn ls(_repo: Option<String>) -> anyhow::Result<()> {
    anyhow::bail!("`shoka tag ls` is not implemented yet")
}

pub async fn who(_tag: Option<String>) -> anyhow::Result<()> {
    anyhow::bail!("`shoka tag who` is not implemented yet")
}
