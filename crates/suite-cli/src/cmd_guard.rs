use std::path::Path;

use anyhow::Result;
use clap::Args;

#[derive(Args, Default)]
pub struct ValidateArgs {}

#[derive(Args)]
pub struct CheckArgs {
    /// Path to packet JSON file
    #[arg(long)]
    packet: String,
}

pub fn run_validate(_args: ValidateArgs, config_path: &str) -> Result<i32> {
    let result = guardy_core::validate_config_file(Path::new(config_path))?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(if result.valid { 0 } else { 1 })
}

pub fn run_check(args: CheckArgs, config_path: &str) -> Result<i32> {
    let result = guardy_core::check_packet_file(Path::new(&args.packet), Path::new(config_path))?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(if result.passed { 0 } else { 1 })
}
