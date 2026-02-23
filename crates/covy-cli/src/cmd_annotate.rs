use anyhow::Result;
use clap::Args;

#[derive(Args)]
pub struct AnnotateArgs {}

pub fn run(_args: AnnotateArgs, _config_path: &str) -> Result<i32> {
    anyhow::bail!("`covy annotate` is not implemented yet")
}
