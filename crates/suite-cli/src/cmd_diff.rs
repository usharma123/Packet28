use std::path::Path;
use std::path::PathBuf;

use anyhow::{anyhow, Result};
use clap::Args;
use roaring::RoaringBitmap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use suite_foundation_core::config::{GateConfig, IssueGateConfig};
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DiffAnalyzeKernelInput {
    base: String,
    head: String,
    fail_under_changed: Option<f64>,
    fail_under_total: Option<f64>,
    fail_under_new: Option<f64>,
    max_new_errors: Option<u32>,
    max_new_warnings: Option<u32>,
    max_new_issues: Option<u32>,
    issues: Vec<String>,
    issues_state: Option<String>,
    no_issues_state: bool,
    coverage: Vec<String>,
    input: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DiffAnalyzeKernelOutput {
    gate_result: suite_packet_core::QualityGateResult,
    diagnostics: Option<suite_packet_core::DiagnosticsData>,
    diffs: Vec<SerializableFileDiff>,
}

impl Default for DiffAnalyzeKernelOutput {
    fn default() -> Self {
        Self {
            gate_result: suite_packet_core::QualityGateResult {
                passed: false,
                total_coverage_pct: None,
                changed_coverage_pct: None,
                new_file_coverage_pct: None,
                violations: Vec::new(),
                issue_counts: None,
            },
            diagnostics: None,
            diffs: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SerializableFileDiff {
    path: String,
    old_path: Option<String>,
    status: suite_packet_core::DiffStatus,
    changed_lines: Vec<u32>,
}

impl SerializableFileDiff {
    fn from_file_diff(diff: &suite_packet_core::FileDiff) -> Self {
        Self {
            path: diff.path.clone(),
            old_path: diff.old_path.clone(),
            status: diff.status,
            changed_lines: diff.changed_lines.iter().collect(),
        }
    }

    fn into_file_diff(self) -> suite_packet_core::FileDiff {
        let mut bitmap = RoaringBitmap::new();
        for line in self.changed_lines {
            bitmap.insert(line);
        }

        suite_packet_core::FileDiff {
            path: self.path,
            old_path: self.old_path,
            status: self.status,
            changed_lines: bitmap,
        }
    }
}

fn format_pct(value: Option<f64>) -> String {
    value
        .map(|pct| format!("{pct:.2}"))
        .unwrap_or_else(|| "n/a".to_string())
}

pub fn run(args: AnalyzeArgs, config_path: &str) -> Result<i32> {
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
    let config = CovyConfig::load(Path::new(config_path)).unwrap_or_default();
    let machine_profile =
        crate::cmd_common::resolve_machine_profile(args.json, args.report.as_deref(), "--report")?;
    let report = if machine_profile.is_some() {
        "json".to_string()
    } else {
        crate::cmd_common::resolve_report_format(args.report.as_deref())
    };

    let base = args.base.as_deref().unwrap_or(&config.diff.base);
    let head = args.head.as_deref().unwrap_or(&config.diff.head);

    let kernel_input = DiffAnalyzeKernelInput {
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

    if machine_profile.is_some() && !args.legacy_json && !args.cache && governed_context_config.is_none()
    {
        let request = build_pipeline_request(&kernel_input);
        let adapters = crate::cmd_common::default_pipeline_ingest_adapters();
        let output = diffy_core::pipeline::run_analysis(request, &adapters)?;
        let envelope = build_diff_envelope(&output, base, head);
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

    let mut kernel = build_kernel(args.cache, std::env::current_dir()?);
    kernel.register_reducer("diffy.analyze", run_diff_analyze_reducer);
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
    let envelope: suite_packet_core::EnvelopeV1<DiffAnalyzeKernelOutput> =
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
                    .map(SerializableFileDiff::into_file_diff)
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

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn build_pipeline_request(input: &DiffAnalyzeKernelInput) -> diffy_core::pipeline::PipelineRequest {
    diffy_core::pipeline::PipelineRequest {
        base: input.base.clone(),
        head: input.head.clone(),
        source_root: None,
        coverage: diffy_core::pipeline::PipelineCoverageInput {
            paths: input.coverage.clone(),
            format: None,
            stdin: false,
            input_state_path: input.input.clone(),
            default_input_state_path: Some(".covy/state/latest.bin".to_string()),
            strip_prefixes: Vec::new(),
            reject_paths_with_input: false,
            no_inputs_error: "No coverage data found. Run `covy ingest` first or use --coverage."
                .to_string(),
        },
        diagnostics: diffy_core::pipeline::PipelineDiagnosticsInput {
            issue_patterns: input.issues.clone(),
            issues_state_path: input.issues_state.clone(),
            no_issues_state: input.no_issues_state,
            default_issues_state_path: ".covy/state/issues.bin".to_string(),
        },
        gate: GateConfig {
            fail_under_total: input.fail_under_total,
            fail_under_changed: input.fail_under_changed,
            fail_under_new: input.fail_under_new,
            issues: IssueGateConfig {
                max_new_errors: input.max_new_errors,
                max_new_warnings: input.max_new_warnings,
                max_new_issues: input.max_new_issues,
            },
        },
    }
}

fn build_diff_envelope(
    output: &diffy_core::pipeline::PipelineOutput,
    base: &str,
    head: &str,
) -> suite_packet_core::EnvelopeV1<DiffAnalyzeKernelOutput> {
    let kernel_output = DiffAnalyzeKernelOutput {
        gate_result: output.gate_result.clone(),
        diagnostics: output.diagnostics.clone(),
        diffs: output
            .changed_line_context
            .diffs
            .iter()
            .map(SerializableFileDiff::from_file_diff)
            .collect(),
    };

    let mut changed_paths = output
        .changed_line_context
        .changed_paths
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    changed_paths.sort();

    let gate_summary = format!(
        "passed: {}\nchanged_coverage_pct: {}\ntotal_coverage_pct: {}\nnew_file_coverage_pct: {}\nviolations: {}",
        kernel_output.gate_result.passed,
        format_pct(kernel_output.gate_result.changed_coverage_pct),
        format_pct(kernel_output.gate_result.total_coverage_pct),
        format_pct(kernel_output.gate_result.new_file_coverage_pct),
        if kernel_output.gate_result.violations.is_empty() {
            "none".to_string()
        } else {
            kernel_output.gate_result.violations.join("; ")
        }
    );

    let changed_file_body = if changed_paths.is_empty() {
        "No changed files".to_string()
    } else {
        changed_paths.join("\n")
    };

    let files = changed_paths
        .iter()
        .map(|path| suite_packet_core::FileRef {
            path: path.clone(),
            relevance: Some(0.75),
            source: Some("diffy.analyze".to_string()),
        })
        .collect::<Vec<_>>();
    let payload_bytes = serde_json::to_vec(&kernel_output).unwrap_or_default().len();

    suite_packet_core::EnvelopeV1 {
        version: "1".to_string(),
        tool: "diffy".to_string(),
        kind: "diff_analyze".to_string(),
        hash: String::new(),
        summary: format!("{gate_summary}\nchanged_files: {changed_file_body}"),
        files,
        symbols: Vec::new(),
        risk: None,
        confidence: Some(if output.gate_result.passed { 1.0 } else { 0.8 }),
        budget_cost: suite_packet_core::BudgetCost {
            est_tokens: 0,
            est_bytes: 0,
            runtime_ms: 0,
            tool_calls: 1,
            payload_est_tokens: Some((payload_bytes / 4) as u64),
            payload_est_bytes: Some(payload_bytes),
        },
        provenance: suite_packet_core::Provenance {
            inputs: changed_paths,
            git_base: Some(base.to_string()),
            git_head: Some(head.to_string()),
            generated_at_unix: now_unix(),
        },
        payload: kernel_output,
    }
    .with_canonical_hash_and_real_budget()
}

fn run_diff_analyze_reducer(
    ctx: &mut context_kernel_core::ExecutionContext,
    _input_packets: &[context_kernel_core::KernelPacket],
) -> Result<context_kernel_core::ReducerResult, context_kernel_core::KernelError> {
    let input: DiffAnalyzeKernelInput =
        serde_json::from_value(ctx.reducer_input.clone()).map_err(|source| {
            context_kernel_core::KernelError::ReducerFailed {
                target: ctx.target.clone(),
                detail: format!("invalid reducer input: {source}"),
            }
        })?;

    let git_base = input.base.clone();
    let git_head = input.head.clone();
    let request = build_pipeline_request(&input);
    let adapters = crate::cmd_common::default_pipeline_ingest_adapters();
    let output = diffy_core::pipeline::run_analysis(request, &adapters).map_err(|source| {
        context_kernel_core::KernelError::ReducerFailed {
            target: ctx.target.clone(),
            detail: source.to_string(),
        }
    })?;
    let envelope = build_diff_envelope(&output, &git_base, &git_head);

    Ok(context_kernel_core::ReducerResult {
        output_packets: vec![context_kernel_core::KernelPacket {
            packet_id: Some(format!(
                "diffy-{}",
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
                "reducer": "diffy.analyze",
                "kind": "diff_analyze",
                "hash": envelope.hash,
                "passed": output.gate_result.passed,
            }),
        }],
        metadata: json!({
            "reducer": "diffy.analyze",
            "kind": "diff_analyze",
            "passed": output.gate_result.passed,
        }),
    })
}
