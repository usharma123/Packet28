use anyhow::Result;
use clap::Args;

#[derive(Args)]
pub struct DoctorArgs {}

pub fn run(_args: DoctorArgs, _config_path: &str) -> Result<i32> {
    anyhow::bail!("`covy doctor` is not implemented yet")
}
