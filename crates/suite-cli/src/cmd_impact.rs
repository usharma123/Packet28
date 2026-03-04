use anyhow::{anyhow, Result};
use clap::Args;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;

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

    /// Persist kernel cache on disk under <cwd>/.packet28
    #[arg(long)]
    pub cache: bool,

    /// Run governed packet path using this context policy config (context.yaml).
    #[arg(long)]
    pub context_config: Option<String>,

    /// Context assembly token budget for governed mode.
    #[arg(long, default_value_t = contextq_core::DEFAULT_BUDGET_TOKENS)]
    pub context_budget_tokens: u64,

    /// Context assembly byte budget for governed mode.
    #[arg(long, default_value_t = contextq_core::DEFAULT_BUDGET_BYTES)]
    pub context_budget_bytes: usize,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ImpactKernelPacket {
    packet_id: Option<String>,
    tool: Option<String>,
    reducer: Option<String>,
    paths: Vec<String>,
    payload: ImpactKernelOutput,
}

fn parse_impact_output(body: &Value) -> Result<ImpactKernelOutput> {
    if let Ok(packet) = serde_json::from_value::<ImpactKernelPacket>(body.clone()) {
        return Ok(packet.payload);
    }

    serde_json::from_value(body.clone())
        .map_err(|source| anyhow!("invalid impact output packet: {source}"))
}

pub fn run(args: ImpactArgs, config_path: &str) -> Result<i32> {
    let governed_context_config = args.context_config.clone();
    let governed_budget_tokens = args.context_budget_tokens;
    let governed_budget_bytes = args.context_budget_bytes;
    let policy_context = governed_context_config
        .as_ref()
        .map(|config_path| {
            json!({
                "config_path": config_path,
            })
        })
        .unwrap_or(Value::Null);

    let mut kernel = build_kernel(args.cache, std::env::current_dir()?);
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
        policy_context: policy_context.clone(),
        ..context_kernel_core::KernelRequest::default()
    })?;

    let output_packet = response
        .output_packets
        .first()
        .ok_or_else(|| anyhow!("kernel returned no output packets"))?;
    let output = parse_impact_output(&output_packet.body)?;

    let governed_response = if governed_context_config.is_some() {
        Some(kernel.execute(context_kernel_core::KernelRequest {
            target: "governed.assemble".to_string(),
            input_packets: vec![output_packet.clone()],
            budget: context_kernel_core::ExecutionBudget {
                token_cap: Some(governed_budget_tokens),
                byte_cap: Some(governed_budget_bytes),
                runtime_ms_cap: None,
            },
            policy_context,
            ..context_kernel_core::KernelRequest::default()
        })?)
    } else {
        None
    };

    if args.json {
        if let Some(governed) = governed_response {
            let budget_hint = crate::cmd_common::budget_retry_hint(
                &governed.metadata,
                governed_budget_tokens,
                governed_budget_bytes,
                "Packet28 test impact --context-config <context.yaml>",
            );
            let final_packet = governed
                .output_packets
                .first()
                .ok_or_else(|| anyhow!("kernel returned no output packets for governed flow"))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "schema_version": "suite.test.impact.v1",
                    "impact_result": output.result,
                    "known_tests": output.known_tests,
                    "print_command": output.print_command,
                    "final_packet": final_packet.body,
                    "kernel_audit": {
                        "impact": response.audit,
                        "governed": governed.audit,
                    },
                    "kernel_metadata": {
                        "impact": response.metadata,
                        "governed": governed.metadata,
                    },
                    "cache": {
                        "impact": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
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
                    "schema_version": "suite.test.impact.v1",
                    "impact_result": output.result,
                    "known_tests": output.known_tests,
                    "print_command": output.print_command,
                    "kernel_audit": {
                        "impact": response.audit,
                    },
                    "kernel_metadata": {
                        "impact": response.metadata,
                    },
                    "cache": {
                        "impact": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
                    },
                }))?
            );
        }
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

    if let Some(governed) = governed_response {
        if let Some(summary) = crate::cmd_common::cache_summary_line(&response.metadata) {
            println!("{summary}");
        }
        if let Some(summary) = crate::cmd_common::cache_summary_line(&governed.metadata) {
            println!("{summary}");
        }
        if let Some(hint) = crate::cmd_common::budget_retry_hint(
            &governed.metadata,
            governed_budget_tokens,
            governed_budget_bytes,
            "Packet28 test impact --context-config <context.yaml>",
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
    } else if let Some(summary) = crate::cmd_common::cache_summary_line(&response.metadata) {
        println!("{summary}");
    }

    Ok(0)
}

fn build_kernel(cache: bool, root_dir: PathBuf) -> context_kernel_core::Kernel {
    if cache {
        return context_kernel_core::Kernel::with_v1_reducers_and_persistence(
            context_kernel_core::PersistConfig::new(root_dir),
        );
    }
    context_kernel_core::Kernel::with_v1_reducers()
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

    let impact_output = ImpactKernelOutput {
        result: output.result.clone(),
        known_tests: output.known_tests,
        print_command: output.print_command.clone(),
    };

    let mut paths = output.result.missing_mappings.clone();
    paths.sort();
    paths.dedup();

    let mut symbol_refs = output.result.selected_tests.clone();
    symbol_refs.extend(output.result.smoke_tests.clone());
    symbol_refs.sort();
    symbol_refs.dedup();

    let file_refs = paths
        .iter()
        .map(|path| {
            json!({
                "kind": "file",
                "value": path,
                "source": "testy-impact-v1",
                "relevance": 0.8
            })
        })
        .collect::<Vec<_>>();
    let selected_refs = output
        .result
        .selected_tests
        .iter()
        .map(|test| {
            json!({
                "kind": "symbol",
                "value": test,
                "source": "testy-impact-v1",
                "relevance": 0.9
            })
        })
        .collect::<Vec<_>>();
    let symbol_refs_payload = symbol_refs
        .iter()
        .map(|symbol| {
            json!({
                "kind": "symbol",
                "value": symbol,
                "source": "testy-impact-v1",
                "relevance": 0.7
            })
        })
        .collect::<Vec<_>>();

    let mut refs = file_refs.clone();
    refs.extend(symbol_refs_payload.clone());

    let selected_tests_body = if output.result.selected_tests.is_empty() {
        "(no impacted tests)".to_string()
    } else {
        output.result.selected_tests.join("\n")
    };
    let smoke_tests_body = if output.result.smoke_tests.is_empty() {
        "(none)".to_string()
    } else {
        output.result.smoke_tests.join("\n")
    };

    let summary = format!(
        "selected: {}\nknown: {}\nmissing: {}\nconfidence: {:.2}\nstale: {}\nescalate_full_suite: {}",
        output.result.selected_tests.len(),
        output.known_tests,
        output.result.missing_mappings.len(),
        output.result.confidence,
        output.result.stale,
        output.result.escalate_full_suite,
    );

    let mut sections = vec![
        json!({
            "id": "impact-summary",
            "title": "Impact Summary",
            "body": summary,
            "refs": refs.clone(),
            "relevance": if output.result.escalate_full_suite { 1.2 } else { 0.85 },
        }),
        json!({
            "id": "selected-tests",
            "title": "Selected Tests",
            "body": selected_tests_body,
            "refs": selected_refs,
            "relevance": 1.0,
        }),
    ];

    if !output.result.smoke_tests.is_empty() {
        sections.push(json!({
            "id": "smoke-tests",
            "title": "Smoke Tests",
            "body": smoke_tests_body,
            "refs": symbol_refs_payload,
            "relevance": 0.8,
        }));
    }

    let packet_body = json!({
        "packet_id": "testy-impact-v1",
        "tool": "testy",
        "tools": ["testy"],
        "reducer": "impact",
        "reducers": ["impact"],
        "paths": paths,
        "payload": impact_output,
        "sections": sections,
        "refs": refs,
        "text_blobs": [summary],
    });

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
