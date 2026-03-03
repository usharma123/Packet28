use anyhow::{anyhow, Result};
use clap::Args;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Args)]
pub struct ImpactArgs {
    /// Base ref for diff (default: main)
    #[arg(long)]
    pub base: Option<String>,

    /// Head ref for diff (default: HEAD)
    #[arg(long)]
    pub head: Option<String>,

    /// Path to testmap state
    #[arg(long, default_value = ".covy/state/testmap.bin")]
    pub testmap: String,

    /// Emit JSON output
    #[arg(long)]
    pub json: bool,

    /// Emit runnable test command
    #[arg(long)]
    pub print_command: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ImpactKernelInput {
    base: Option<String>,
    head: Option<String>,
    testmap: String,
    print_command: bool,
    config_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ImpactKernelOutput {
    result: suite_packet_core::ImpactResult,
    known_tests: usize,
    print_command: Option<String>,
}

pub fn run(args: ImpactArgs, config_path: &str) -> Result<i32> {
    let mut kernel = context_kernel_core::Kernel::with_v1_reducers();
    kernel.register_reducer("testy.impact", run_test_impact_reducer);
    let response = kernel.execute(context_kernel_core::KernelRequest {
        target: "testy.impact".to_string(),
        reducer_input: serde_json::to_value(ImpactKernelInput {
            base: args.base,
            head: args.head,
            testmap: args.testmap,
            print_command: args.print_command,
            config_path: config_path.to_string(),
        })?,
        ..context_kernel_core::KernelRequest::default()
    })?;

    let output_packet = response
        .output_packets
        .first()
        .ok_or_else(|| anyhow!("kernel returned no output packets"))?;
    let output: ImpactKernelOutput = serde_json::from_value(output_packet.body.clone())?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&output.result)?);
        return Ok(0);
    }

    if output.result.selected_tests.is_empty() {
        println!("(no impacted tests)");
    } else {
        for test in &output.result.selected_tests {
            println!("{test}");
        }
    }
    println!(
        "summary: selected={} known={} missing={} confidence={:.2} stale={} escalate_full_suite={}",
        output.result.selected_tests.len(),
        output.known_tests,
        output.result.missing_mappings.len(),
        output.result.confidence,
        output.result.stale,
        output.result.escalate_full_suite
    );

    if args.print_command {
        if let Some(command) = output.print_command {
            println!("{command}");
        }
    }

    Ok(0)
}

fn run_test_impact_reducer(
    ctx: &mut context_kernel_core::ExecutionContext,
    _input_packets: &[context_kernel_core::KernelPacket],
) -> Result<context_kernel_core::ReducerResult, context_kernel_core::KernelError> {
    let input: ImpactKernelInput =
        serde_json::from_value(ctx.reducer_input.clone()).map_err(|source| {
            context_kernel_core::KernelError::ReducerFailed {
                target: ctx.target.clone(),
                detail: format!("invalid reducer input: {source}"),
            }
        })?;

    let adapters = testy_cli_common::adapters::default_impact_adapters();
    let output = testy_core::command_impact::run_legacy_impact(
        testy_core::command_impact::LegacyImpactArgs {
            base: input.base,
            head: input.head,
            testmap: input.testmap,
            print_command: input.print_command,
        },
        &input.config_path,
        &adapters,
    )
    .map_err(|source| context_kernel_core::KernelError::ReducerFailed {
        target: ctx.target.clone(),
        detail: source.to_string(),
    })?;

    let packet_body = serde_json::to_value(ImpactKernelOutput {
        result: output.result.clone(),
        known_tests: output.known_tests,
        print_command: output.print_command.clone(),
    })
    .map_err(|source| context_kernel_core::KernelError::ReducerFailed {
        target: ctx.target.clone(),
        detail: source.to_string(),
    })?;

    Ok(context_kernel_core::ReducerResult {
        output_packets: vec![context_kernel_core::KernelPacket {
            packet_id: Some("testy-impact-v1".to_string()),
            format: "packet-json".to_string(),
            body: packet_body,
            token_usage: None,
            runtime_ms: None,
            metadata: json!({
                "reducer": "testy.impact",
                "selected_tests": output.result.selected_tests.len(),
            }),
        }],
        metadata: json!({
            "reducer": "testy.impact",
            "selected_tests": output.result.selected_tests.len(),
        }),
    })
}
