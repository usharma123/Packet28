use std::path::Path;

use anyhow::{anyhow, Result};
use clap::Args;
use serde_json::{json, Value};

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

    /// Run governed assembly path using this context policy config (context.yaml).
    #[arg(long)]
    context_config: Option<String>,
}

pub fn run_assemble(args: AssembleArgs) -> Result<i32> {
    let input_packets = args
        .packets
        .iter()
        .map(|path| context_kernel_core::load_packet_file(Path::new(path)))
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let kernel = context_kernel_core::Kernel::with_v1_reducers();
    let target = if args.context_config.is_some() {
        "governed.assemble"
    } else {
        "contextq.assemble"
    };
    let response = kernel.execute(context_kernel_core::KernelRequest {
        target: target.to_string(),
        input_packets,
        budget: context_kernel_core::ExecutionBudget {
            token_cap: Some(args.budget_tokens),
            byte_cap: Some(args.budget_bytes),
            runtime_ms_cap: None,
        },
        policy_context: args
            .context_config
            .as_ref()
            .map(|config_path| json!({ "config_path": config_path }))
            .unwrap_or(Value::Null),
        ..context_kernel_core::KernelRequest::default()
    })?;

    let assembled = response
        .output_packets
        .first()
        .ok_or_else(|| anyhow!("kernel returned no output packets"))?;

    if args.context_config.is_some() {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "schema_version": "suite.context.assemble.v1",
                "final_packet": assembled.body,
                "kernel_audit": {
                    "governed": response.audit,
                },
                "kernel_metadata": {
                    "governed": response.metadata,
                }
            }))?
        );
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "schema_version": "suite.context.assemble.v1",
                "packet": assembled.body,
                "kernel_audit": {
                    "context": response.audit,
                },
                "kernel_metadata": {
                    "context": response.metadata,
                }
            }))?
        );
    }

    Ok(0)
}
