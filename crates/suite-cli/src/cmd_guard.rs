use std::path::Path;

use anyhow::{anyhow, Result};
use clap::Args;
use serde_json::json;

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
    let packet = context_kernel_core::load_packet_file(Path::new(&args.packet))?;
    let kernel = context_kernel_core::Kernel::with_v1_reducers();
    let response = kernel.execute(context_kernel_core::KernelRequest {
        target: "guardy.check".to_string(),
        input_packets: vec![packet],
        policy_context: json!({
            "config_path": config_path,
        }),
        ..context_kernel_core::KernelRequest::default()
    })?;

    let audit_packet = response
        .output_packets
        .first()
        .ok_or_else(|| anyhow!("kernel returned no output packets"))?;
    let audit: guardy_core::AuditResult = serde_json::from_value(audit_packet.body.clone())?;
    println!("{}", serde_json::to_string_pretty(&audit)?);
    Ok(if audit.passed { 0 } else { 1 })
}
