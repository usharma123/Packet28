use anyhow::Result;
use clap::Args;

#[derive(Args)]
pub struct PrArgs {}

pub fn run(_args: PrArgs, _config_path: &str) -> Result<i32> {
    anyhow::bail!("`covy pr` is not implemented yet")
}
