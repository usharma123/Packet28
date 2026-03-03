use std::path::Path;

use anyhow::{anyhow, Result};
use clap::Args;

#[derive(Args)]
pub struct AssembleArgs {
    /// Path(s) to reducer packet JSON files.
    #[arg(long = "packet", alias = "input", required = true)]
    packets: Vec<String>,

    /// Max approximate token budget for assembled payload.
    #[arg(long, default_value_t = contextq_core::DEFAULT_BUDGET_TOKENS)]
    budget_tokens: u64,

    /// Max byte budget for assembled payload JSON.
    #[arg(long, default_value_t = contextq_core::DEFAULT_BUDGET_BYTES)]
    budget_bytes: usize,
}

pub fn run_assemble(args: AssembleArgs) -> Result<i32> {
    let input_packets = args
        .packets
        .iter()
        .map(|path| context_kernel_core::load_packet_file(Path::new(path)))
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let kernel = context_kernel_core::Kernel::with_v1_reducers();
    let response = kernel.execute(context_kernel_core::KernelRequest {
        target: "contextq.assemble".to_string(),
        input_packets,
        budget: context_kernel_core::ExecutionBudget {
            token_cap: Some(args.budget_tokens),
            byte_cap: Some(args.budget_bytes),
            runtime_ms_cap: None,
        },
        ..context_kernel_core::KernelRequest::default()
    })?;

    let assembled = response
        .output_packets
        .first()
        .ok_or_else(|| anyhow!("kernel returned no output packets"))?;
    println!("{}", serde_json::to_string_pretty(&assembled.body)?);
    Ok(0)
}
