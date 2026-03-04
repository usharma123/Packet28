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
    #[arg(long, value_enum, num_args = 0..=1, default_missing_value = "compact")]
    pub json: Option<crate::cmd_common::JsonProfileArg>,

    /// Emit one-release compatibility JSON shape
    #[arg(long)]
    pub legacy_json: bool,

    /// Pretty-print JSON output
    #[arg(long)]
    pub pretty: bool,

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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ImpactKernelOutput {
    result: suite_packet_core::ImpactResult,
    known_tests: usize,
    print_command: Option<String>,
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
    let envelope: suite_packet_core::EnvelopeV1<ImpactKernelOutput> =
        serde_json::from_value(output_packet.body.clone())
            .map_err(|source| anyhow!("invalid impact output packet: {source}"))?;
    let output = envelope.payload.clone();

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

    if let Some(profile_arg) = args.json {
        let profile: suite_packet_core::JsonProfile = profile_arg.into();
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
            if args.legacy_json {
                crate::cmd_common::emit_json(
                    &json!({
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
                    }),
                    args.pretty,
                )?;
            } else {
                crate::cmd_common::emit_machine_envelope(
                    suite_packet_core::PACKET_TYPE_TEST_IMPACT,
                    &envelope,
                    profile,
                    args.pretty,
                    &crate::cmd_common::resolve_artifact_root(None),
                    Some(json!({
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
                        "governed_packet": final_packet.body,
                    })),
                )?;
            }
        } else {
            if args.legacy_json {
                crate::cmd_common::emit_json(
                    &json!({
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
                    }),
                    args.pretty,
                )?;
            } else {
                crate::cmd_common::emit_machine_envelope(
                    suite_packet_core::PACKET_TYPE_TEST_IMPACT,
                    &envelope,
                    profile,
                    args.pretty,
                    &crate::cmd_common::resolve_artifact_root(None),
                    Some(json!({
                        "kernel_audit": {
                            "impact": response.audit,
                        },
                        "kernel_metadata": {
                            "impact": response.metadata,
                        },
                        "cache": {
                            "impact": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
                        },
                    })),
                )?;
            }
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
            .get("payload")
            .and_then(|payload| payload.get("assembly"))
            .and_then(|assembly| assembly.get("sections_kept"))
            .or_else(|| {
                final_packet
                    .body
                    .get("assembly")
                    .and_then(|assembly| assembly.get("sections_kept"))
            })
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

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
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

    let testmap_path = input.testmap.clone();
    let git_base = input.base.clone();
    let git_head = input.head.clone();
    let adapters = testy_cli_common::adapters::default_impact_adapters();
    let output = testy_core::command_impact::run_legacy_impact(
        testy_core::command_impact::LegacyImpactArgs {
            base: input.base.clone(),
            head: input.head.clone(),
            testmap: input.testmap.clone(),
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

    let summary = format!(
        "selected: {}\nknown: {}\nmissing: {}\nconfidence: {:.2}\nstale: {}\nescalate_full_suite: {}",
        output.result.selected_tests.len(),
        output.known_tests,
        output.result.missing_mappings.len(),
        output.result.confidence,
        output.result.stale,
        output.result.escalate_full_suite,
    );

    let files = paths
        .iter()
        .map(|path| suite_packet_core::FileRef {
            path: path.clone(),
            relevance: Some(0.8),
            source: Some("testy.impact".to_string()),
        })
        .collect::<Vec<_>>();
    let symbols = symbol_refs
        .iter()
        .map(|symbol| suite_packet_core::SymbolRef {
            name: symbol.clone(),
            file: None,
            kind: Some("test_id".to_string()),
            relevance: Some(0.8),
            source: Some("testy.impact".to_string()),
        })
        .collect::<Vec<_>>();

    let payload_bytes = serde_json::to_vec(&impact_output).unwrap_or_default().len();
    let envelope = suite_packet_core::EnvelopeV1 {
        version: "1".to_string(),
        tool: "testy".to_string(),
        kind: "test_impact".to_string(),
        hash: String::new(),
        summary,
        files,
        symbols,
        risk: None,
        confidence: Some(output.result.confidence.clamp(0.0, 1.0)),
        budget_cost: suite_packet_core::BudgetCost {
            est_tokens: 0,
            est_bytes: 0,
            runtime_ms: 0,
            tool_calls: 1,
            payload_est_tokens: Some((payload_bytes / 4) as u64),
            payload_est_bytes: Some(payload_bytes),
        },
        provenance: suite_packet_core::Provenance {
            inputs: vec![testmap_path],
            git_base,
            git_head,
            generated_at_unix: now_unix(),
        },
        payload: impact_output,
    }
    .with_canonical_hash_and_real_budget();

    Ok(context_kernel_core::ReducerResult {
        output_packets: vec![context_kernel_core::KernelPacket {
            packet_id: Some(format!(
                "testy-{}",
                envelope.hash.chars().take(12).collect::<String>()
            )),
            format: "packet-json".to_string(),
            body: serde_json::to_value(&envelope).map_err(|source| {
                context_kernel_core::KernelError::ReducerFailed {
                    target: ctx.target.clone(),
                    detail: source.to_string(),
                }
            })?,
            token_usage: Some(envelope.budget_cost.est_tokens),
            runtime_ms: Some(envelope.budget_cost.runtime_ms),
            metadata: json!({
                "reducer": "testy.impact",
                "kind": "test_impact",
                "hash": envelope.hash,
                "selected_tests": output.result.selected_tests.len(),
            }),
        }],
        metadata: json!({
            "reducer": "testy.impact",
            "kind": "test_impact",
            "selected_tests": output.result.selected_tests.len(),
        }),
    })
}
