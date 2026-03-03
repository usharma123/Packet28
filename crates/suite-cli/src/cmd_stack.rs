use std::io::Read;

use anyhow::{anyhow, Context, Result};
use clap::Args;
use serde_json::{json, Value};

#[derive(Args)]
pub struct SliceArgs {
    /// Input stack trace/log file path (reads stdin when omitted)
    #[arg(long)]
    input: Option<String>,

    /// Emit JSON output
    #[arg(long)]
    json: bool,

    /// Optional cap on number of unique failures in output
    #[arg(long)]
    max_failures: Option<usize>,

    /// Run governed packet path using this context policy config (context.yaml).
    #[arg(long)]
    context_config: Option<String>,

    /// Context assembly token budget for governed mode.
    #[arg(long, default_value_t = contextq_core::DEFAULT_BUDGET_TOKENS)]
    context_budget_tokens: u64,

    /// Context assembly byte budget for governed mode.
    #[arg(long, default_value_t = contextq_core::DEFAULT_BUDGET_BYTES)]
    context_budget_bytes: usize,
}

pub fn run(args: SliceArgs) -> Result<i32> {
    let input_text = read_input_text(args.input.as_deref())?;

    let kernel = context_kernel_core::Kernel::with_v1_reducers();
    let response = kernel.execute(context_kernel_core::KernelRequest {
        target: "stacky.slice".to_string(),
        reducer_input: serde_json::to_value(stacky_core::StackSliceRequest {
            log_text: input_text,
            source: args.input.clone(),
            max_failures: args.max_failures,
        })?,
        policy_context: args
            .context_config
            .as_ref()
            .map(|path| json!({"config_path": path}))
            .unwrap_or(Value::Null),
        ..context_kernel_core::KernelRequest::default()
    })?;

    let output_packet = response
        .output_packets
        .first()
        .ok_or_else(|| anyhow!("kernel returned no output packets"))?;
    let packet: stacky_core::StackPacket = serde_json::from_value(output_packet.body.clone())
        .map_err(|source| anyhow!("invalid stacky output packet: {source}"))?;

    let governed_response = if let Some(context_config) = args.context_config {
        Some(kernel.execute(context_kernel_core::KernelRequest {
            target: "governed.assemble".to_string(),
            input_packets: vec![output_packet.clone()],
            budget: context_kernel_core::ExecutionBudget {
                token_cap: Some(args.context_budget_tokens),
                byte_cap: Some(args.context_budget_bytes),
                runtime_ms_cap: None,
            },
            policy_context: json!({
                "config_path": context_config,
            }),
            ..context_kernel_core::KernelRequest::default()
        })?)
    } else {
        None
    };

    if args.json {
        if let Some(governed) = governed_response {
            let final_packet = governed
                .output_packets
                .first()
                .ok_or_else(|| anyhow!("kernel returned no output packets for governed flow"))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "schema_version": "suite.stack.slice.v1",
                    "packet": packet,
                    "final_packet": final_packet.body,
                    "kernel_audit": {
                        "stack": response.audit,
                        "governed": governed.audit,
                    },
                    "kernel_metadata": {
                        "stack": response.metadata,
                        "governed": governed.metadata,
                    },
                }))?
            );
        } else {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "schema_version": "suite.stack.slice.v1",
                    "packet": packet,
                    "kernel_audit": {
                        "stack": response.audit,
                    },
                    "kernel_metadata": {
                        "stack": response.metadata,
                    },
                }))?
            );
        }
        return Ok(0);
    }

    let payload: stacky_core::StackSliceOutput = serde_json::from_value(packet.payload.clone())
        .map_err(|source| anyhow!("invalid stacky payload: {source}"))?;
    println!(
        "summary: total={} unique={} duplicates_removed={}",
        payload.total_failures, payload.unique_failures, payload.duplicates_removed
    );
    for failure in payload.failures {
        let actionable = failure
            .first_actionable_frame
            .as_ref()
            .and_then(|frame| frame.file.as_deref())
            .unwrap_or("unknown");
        println!(
            "- [{}] {} occurrences={} actionable={}",
            failure.fingerprint, failure.title, failure.occurrences, actionable
        );
    }

    if let Some(governed) = governed_response {
        let final_packet = governed
            .output_packets
            .first()
            .ok_or_else(|| anyhow!("kernel returned no output packets for governed flow"))?;
        let sections = final_packet
            .body
            .get("assembly")
            .and_then(|assembly| assembly.get("sections_kept"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        println!(
            "governed packet assembled: packet_id={} sections_kept={sections}",
            final_packet.packet_id.as_deref().unwrap_or("unknown")
        );
    }

    Ok(0)
}

fn read_input_text(path: Option<&str>) -> Result<String> {
    match path {
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("failed to read input file '{path}'")),
        None => {
            let mut buffer = String::new();
            std::io::stdin()
                .read_to_string(&mut buffer)
                .context("failed to read stack input from stdin")?;
            Ok(buffer)
        }
    }
}
