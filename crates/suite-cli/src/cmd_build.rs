use std::io::Read;

use anyhow::{anyhow, Context, Result};
use clap::Args;
use serde_json::{json, Value};
use std::path::PathBuf;

#[derive(Args)]
pub struct ReduceArgs {
    /// Input compiler/linter output file path (reads stdin when omitted)
    #[arg(long)]
    input: Option<String>,

    /// Emit JSON output
    #[arg(long)]
    json: bool,

    /// Optional cap on number of parsed diagnostics
    #[arg(long)]
    max_diagnostics: Option<usize>,

    /// Persist kernel cache on disk under <cwd>/.packet28
    #[arg(long)]
    cache: bool,

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

pub fn run(args: ReduceArgs) -> Result<i32> {
    let input_text = read_input_text(args.input.as_deref())?;

    let kernel = build_kernel(args.cache, std::env::current_dir()?);
    let response = kernel.execute(context_kernel_core::KernelRequest {
        target: "buildy.reduce".to_string(),
        reducer_input: serde_json::to_value(buildy_core::BuildReduceRequest {
            log_text: input_text,
            source: args.input.clone(),
            max_diagnostics: args.max_diagnostics,
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
    let packet: buildy_core::BuildPacket = serde_json::from_value(output_packet.body.clone())
        .map_err(|source| anyhow!("invalid buildy output packet: {source}"))?;

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
            let budget_hint = crate::cmd_common::budget_retry_hint(
                &governed.metadata,
                args.context_budget_tokens,
                args.context_budget_bytes,
                "Packet28 build reduce --context-config <context.yaml>",
            );
            let final_packet = governed
                .output_packets
                .first()
                .ok_or_else(|| anyhow!("kernel returned no output packets for governed flow"))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "schema_version": "suite.build.reduce.v1",
                    "packet": packet,
                    "final_packet": final_packet.body,
                    "kernel_audit": {
                        "build": response.audit,
                        "governed": governed.audit,
                    },
                    "kernel_metadata": {
                        "build": response.metadata,
                        "governed": governed.metadata,
                    },
                    "cache": {
                        "build": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
                        "governed": governed.metadata.get("cache").cloned().unwrap_or(Value::Null),
                    },
                    "hints": {
                        "budget_retry": budget_hint,
                    },
                }))?
            );
        } else {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "schema_version": "suite.build.reduce.v1",
                    "packet": packet,
                    "kernel_audit": {
                        "build": response.audit,
                    },
                    "kernel_metadata": {
                        "build": response.metadata,
                    },
                    "cache": {
                        "build": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
                    },
                }))?
            );
        }
        return Ok(0);
    }

    let payload: buildy_core::BuildReduceOutput = serde_json::from_value(packet.payload.clone())
        .map_err(|source| anyhow!("invalid buildy payload: {source}"))?;
    println!(
        "summary: total={} unique={} duplicates_removed={}",
        payload.total_diagnostics, payload.unique_diagnostics, payload.duplicates_removed
    );
    for fix in payload.ordered_fixes {
        println!("- {fix}");
    }
    if let Some(summary) = crate::cmd_common::cache_summary_line(&response.metadata) {
        println!("{summary}");
    }

    if let Some(governed) = governed_response {
        if let Some(summary) = crate::cmd_common::cache_summary_line(&governed.metadata) {
            println!("{summary}");
        }
        if let Some(hint) = crate::cmd_common::budget_retry_hint(
            &governed.metadata,
            args.context_budget_tokens,
            args.context_budget_bytes,
            "Packet28 build reduce --context-config <context.yaml>",
        ) {
            if let Some(retry) = hint.get("retry_command").and_then(Value::as_str) {
                println!("hint: high truncation detected; retry with: {retry}");
            }
        }
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
                .context("failed to read build input from stdin")?;
            Ok(buffer)
        }
    }
}

fn build_kernel(cache: bool, root_dir: PathBuf) -> context_kernel_core::Kernel {
    if cache {
        return context_kernel_core::Kernel::with_v1_reducers_and_persistence(
            context_kernel_core::PersistConfig::new(root_dir),
        );
    }
    context_kernel_core::Kernel::with_v1_reducers()
}
