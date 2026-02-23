use anyhow::Result;
use clap::Args;

#[derive(Args)]
pub struct CommentArgs {}

pub fn run(_args: CommentArgs, _config_path: &str) -> Result<i32> {
    anyhow::bail!("`covy comment` is not implemented yet")
}
