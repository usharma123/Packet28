use std::path::Path;
use std::path::PathBuf;

use anyhow::{anyhow, Result};
use clap::Args;
use serde_json::{json, Value};
use suite_foundation_core::CovyConfig;

#[derive(Args)]
pub struct AnalyzeArgs {
    /// Base ref (default: main)
    #[arg(long)]
    base: Option<String>,

    /// Head ref (default: HEAD)
    #[arg(long)]
    head: Option<String>,

    /// Fail if changed lines coverage is below this %
    #[arg(long)]
    fail_under_changed: Option<f64>,

    /// Fail if total coverage is below this %
    #[arg(long)]
    fail_under_total: Option<f64>,

    /// Fail if new file coverage is below this %
    #[arg(long)]
    fail_under_new: Option<f64>,

    /// SARIF diagnostics file paths (supports globs)
    #[arg(long)]
    issues: Vec<String>,

    /// Path to cached diagnostics state file (default: .covy/state/issues.bin)
    #[arg(long)]
    issues_state: Option<String>,

    /// Disable automatic diagnostics state loading when --issues is not provided
    #[arg(long)]
    no_issues_state: bool,

    /// Output format (terminal/json). Defaults to "terminal".
    #[arg(long)]
    report: Option<String>,

    /// Emit JSON output
    #[arg(long, value_enum, num_args = 0..=1, default_missing_value = "compact")]
    json: Option<crate::cmd_common::JsonProfileArg>,

    /// Emit one-release compatibility JSON shape
    #[arg(long)]
    legacy_json: bool,

    /// Pretty-print JSON output
    #[arg(long)]
    pretty: bool,

    /// Coverage report files to ingest (instead of loading state)
    #[arg(long)]
    coverage: Vec<String>,

    /// Path to coverage state file
    #[arg(long)]
    input: Option<String>,

    /// Persist kernel cache on disk under <cwd>/.packet28
    #[arg(long)]
    cache: bool,

    /// Optional task identifier for agent-state propagation.
    #[arg(long)]
    task_id: Option<String>,

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

pub fn run(args: AnalyzeArgs, config_path: &str) -> Result<i32> {
    let governed_context_config = args.context_config.clone();
    let governed_budget_tokens = args.context_budget_tokens;
    let governed_budget_bytes = args.context_budget_bytes;
    let config = CovyConfig::load(Path::new(config_path)).unwrap_or_default();
    let cwd = std::env::current_dir()?;
    let machine_profile =
        crate::cmd_common::resolve_machine_profile(args.json, args.report.as_deref(), "--report")?;
    let report = if machine_profile.is_some() {
        "json".to_string()
    } else {
        crate::cmd_common::resolve_report_format(args.report.as_deref())
    };

    let base = args.base.as_deref().unwrap_or(&config.diff.base);
    let head = args.head.as_deref().unwrap_or(&config.diff.head);

    let kernel_input = context_kernel_core::DiffAnalyzeKernelInput {
        base: base.to_string(),
        head: head.to_string(),
        fail_under_changed: args.fail_under_changed.or(config.gate.fail_under_changed),
        fail_under_total: args.fail_under_total.or(config.gate.fail_under_total),
        fail_under_new: args.fail_under_new.or(config.gate.fail_under_new),
        max_new_errors: config.gate.issues.max_new_errors,
        max_new_warnings: config.gate.issues.max_new_warnings,
        max_new_issues: config.gate.issues.max_new_issues,
        issues: args.issues,
        issues_state: args.issues_state,
        no_issues_state: args.no_issues_state,
        coverage: args.coverage,
        input: args.input,
    };
    let cache_fingerprint = crate::cmd_common::repo_cache_fingerprint(
        &cwd,
        &diff_cache_fingerprint_paths(&kernel_input, &cwd),
    );
    let policy_context = match (governed_context_config.as_ref(), args.task_id.as_ref()) {
        (Some(config_path), Some(task_id)) => json!({
            "config_path": config_path,
            "task_id": task_id,
            "cache_fingerprint": cache_fingerprint,
        }),
        (Some(config_path), None) => json!({
            "config_path": config_path,
            "cache_fingerprint": cache_fingerprint,
        }),
        (None, Some(task_id)) => json!({
            "task_id": task_id,
            "cache_fingerprint": cache_fingerprint,
        }),
        (None, None) => json!({
            "cache_fingerprint": cache_fingerprint,
        }),
    };

    if machine_profile.is_some()
        && !args.legacy_json
        && !args.cache
        && args.task_id.is_none()
        && governed_context_config.is_none()
    {
        let request = context_kernel_core::build_diff_pipeline_request(&kernel_input);
        let adapters = crate::cmd_common::default_pipeline_ingest_adapters();
        let output = diffy_core::pipeline::run_analysis(request, &adapters)?;
        let envelope = context_kernel_core::build_diff_analyze_envelope(&output, base, head);
        crate::cmd_common::emit_machine_envelope(
            suite_packet_core::PACKET_TYPE_DIFF_ANALYZE,
            &envelope,
            machine_profile.unwrap_or(suite_packet_core::JsonProfile::Compact),
            args.pretty,
            &crate::cmd_common::resolve_artifact_root(None),
            None,
        )?;
        return Ok(if output.gate_result.passed { 0 } else { 1 });
    }

    let kernel = build_kernel(args.cache || args.task_id.is_some(), cwd);
    let response = kernel.execute(context_kernel_core::KernelRequest {
        target: "diffy.analyze".to_string(),
        reducer_input: serde_json::to_value(kernel_input)?,
        policy_context: policy_context.clone(),
        ..context_kernel_core::KernelRequest::default()
    })?;

    let output_packet = response
        .output_packets
        .first()
        .ok_or_else(|| anyhow!("kernel returned no output packets"))?;
    let envelope: suite_packet_core::EnvelopeV1<context_kernel_core::DiffAnalyzeKernelOutput> =
        serde_json::from_value(output_packet.body.clone())
            .map_err(|source| anyhow!("invalid diff analyze output packet: {source}"))?;
    let output = envelope.payload.clone();
    let gate_passed = output.gate_result.passed;

    let governed_response = if let Some(context_config) = governed_context_config {
        Some(kernel.execute(context_kernel_core::KernelRequest {
            target: "governed.assemble".to_string(),
            input_packets: vec![output_packet.clone()],
            budget: context_kernel_core::ExecutionBudget {
                token_cap: Some(governed_budget_tokens),
                byte_cap: Some(governed_budget_bytes),
                runtime_ms_cap: None,
            },
            policy_context: json!({
                "config_path": context_config,
                "task_id": args.task_id,
                "disable_cache": args.task_id.is_some(),
            }),
            ..context_kernel_core::KernelRequest::default()
        })?)
    } else {
        None
    };

    match report.as_str() {
        "json" => {
            let profile = machine_profile.unwrap_or(suite_packet_core::JsonProfile::Compact);
            if let Some(governed) = governed_response {
                let budget_hint = crate::cmd_common::budget_retry_hint(
                    &governed.metadata,
                    governed_budget_tokens,
                    governed_budget_bytes,
                    "Packet28 diff analyze --context-config <context.yaml>",
                );
                let final_packet = governed.output_packets.first().ok_or_else(|| {
                    anyhow!("kernel returned no output packets for governed flow")
                })?;
                if args.legacy_json {
                    crate::cmd_common::emit_json(
                        &json!({
                            "schema_version": "suite.diff.analyze.v1",
                            "gate_result": output.gate_result,
                            "diagnostics": output.diagnostics,
                            "diffs": output.diffs,
                            "final_packet": final_packet.body,
                            "kernel_audit": governed.audit,
                            "kernel_metadata": {
                                "diff": response.metadata,
                                "governed": governed.metadata,
                            },
                            "cache": {
                                "diff": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
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
                        suite_packet_core::PACKET_TYPE_DIFF_ANALYZE,
                        &envelope,
                        profile,
                        args.pretty,
                        &crate::cmd_common::resolve_artifact_root(None),
                        Some(json!({
                            "kernel_audit": {
                                "diff": response.audit,
                                "governed": governed.audit,
                            },
                            "kernel_metadata": {
                                "diff": response.metadata,
                                "governed": governed.metadata,
                            },
                            "cache": {
                                "diff": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
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
                    let mut value: Value = serde_json::to_value(&output.gate_result)
                        .map_err(|source| anyhow!("failed to serialize gate json: {source}"))?;
                    if let Some(obj) = value.as_object_mut() {
                        obj.insert(
                            "kernel_metadata".to_string(),
                            json!({
                                "diff": response.metadata,
                            }),
                        );
                        obj.insert(
                            "cache".to_string(),
                            json!({
                                "diff": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
                            }),
                        );
                    }
                    crate::cmd_common::emit_json(&value, args.pretty)?;
                } else {
                    crate::cmd_common::emit_machine_envelope(
                        suite_packet_core::PACKET_TYPE_DIFF_ANALYZE,
                        &envelope,
                        profile,
                        args.pretty,
                        &crate::cmd_common::resolve_artifact_root(None),
                        Some(json!({
                            "cache": {
                                "diff": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
                            },
                            "kernel_metadata": {
                                "diff": response.metadata,
                            },
                        })),
                    )?;
                }
            }
        }
        _ => {
            diffy_core::report::render_gate_result(&output.gate_result);
            if let Some(diag) = output.diagnostics.as_ref() {
                let diffs = output
                    .diffs
                    .iter()
                    .cloned()
                    .map(context_kernel_core::SerializableFileDiff::into_file_diff)
                    .collect::<Vec<_>>();
                diffy_core::report::render_issues_terminal(diag, Some(&diffs));
            }
            if let Some(summary) = crate::cmd_common::cache_summary_line(&response.metadata) {
                println!("{summary}");
            }

            if let Some(governed) = governed_response {
                let final_packet = governed.output_packets.first().ok_or_else(|| {
                    anyhow!("kernel returned no output packets for governed flow")
                })?;
                if let Some(summary) = crate::cmd_common::cache_summary_line(&governed.metadata) {
                    println!("{summary}");
                }
                if let Some(hint) = crate::cmd_common::budget_retry_hint(
                    &governed.metadata,
                    governed_budget_tokens,
                    governed_budget_bytes,
                    "Packet28 diff analyze --context-config <context.yaml>",
                ) {
                    if let Some(retry) = hint.get("retry_command").and_then(Value::as_str) {
                        println!("hint: high truncation detected; retry with: {retry}");
                    }
                }
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
            }
        }
    }

    Ok(if gate_passed { 0 } else { 1 })
}

fn build_kernel(cache: bool, root_dir: PathBuf) -> context_kernel_core::Kernel {
    if cache {
        return context_kernel_core::Kernel::with_v1_reducers_and_persistence(
            context_kernel_core::PersistConfig::new(root_dir),
        );
    }
    context_kernel_core::Kernel::with_v1_reducers()
}

fn diff_cache_fingerprint_paths(
    input: &context_kernel_core::DiffAnalyzeKernelInput,
    cwd: &Path,
) -> Vec<PathBuf> {
    let mut paths = input
        .coverage
        .iter()
        .map(|path| cwd.join(path))
        .collect::<Vec<_>>();
    if let Some(path) = input.input.as_ref() {
        paths.push(cwd.join(path));
    }
    if let Some(path) = input.issues_state.as_ref() {
        paths.push(cwd.join(path));
    }
    paths
}
