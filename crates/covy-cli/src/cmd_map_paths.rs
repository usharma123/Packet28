use anyhow::Result;
use clap::Args;

#[derive(Args)]
pub struct MapPathsArgs {}

pub fn run(_args: MapPathsArgs, _config_path: &str) -> Result<i32> {
    anyhow::bail!("`covy map-paths` is not implemented yet")
}
