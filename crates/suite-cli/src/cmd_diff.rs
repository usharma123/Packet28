use std::path::Path;

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
    #[arg(long)]
    json: bool,

    /// Coverage report files to ingest (instead of loading state)
    #[arg(long)]
    coverage: Vec<String>,

    /// Path to coverage state file
    #[arg(long)]
    input: Option<String>,

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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DiffAnalyzeKernelPacket {
    packet_id: Option<String>,
    tool: Option<String>,
    reducer: Option<String>,
    paths: Vec<String>,
    payload: DiffAnalyzeKernelOutput,
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

fn parse_diff_output(body: &serde_json::Value) -> Result<DiffAnalyzeKernelOutput> {
    if let Ok(packet) = serde_json::from_value::<DiffAnalyzeKernelPacket>(body.clone()) {
        return Ok(packet.payload);
    }

    serde_json::from_value(body.clone())
        .map_err(|source| anyhow!("invalid diff analyze output packet: {source}"))
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
    let report =
        if crate::cmd_common::resolve_json_output(args.json, args.report.as_deref(), "--report")? {
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

    let mut kernel = context_kernel_core::Kernel::with_v1_reducers();
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
    let output = parse_diff_output(&output_packet.body)?;
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
            if let Some(governed) = governed_response {
                let final_packet = governed.output_packets.first().ok_or_else(|| {
                    anyhow!("kernel returned no output packets for governed flow")
                })?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
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
                    }))?
                );
            } else {
                let json = diffy_core::report::render_gate_json(&output.gate_result);
                println!("{json}");
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

            if let Some(governed) = governed_response {
                let final_packet = governed.output_packets.first().ok_or_else(|| {
                    anyhow!("kernel returned no output packets for governed flow")
                })?;
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
        }
    }

    Ok(if gate_passed { 0 } else { 1 })
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

    let request = diffy_core::pipeline::PipelineRequest {
        base: input.base,
        head: input.head,
        source_root: None,
        coverage: diffy_core::pipeline::PipelineCoverageInput {
            paths: input.coverage,
            format: None,
            stdin: false,
            input_state_path: input.input,
            default_input_state_path: Some(".covy/state/latest.bin".to_string()),
            strip_prefixes: Vec::new(),
            reject_paths_with_input: false,
            no_inputs_error: "No coverage data found. Run `covy ingest` first or use --coverage."
                .to_string(),
        },
        diagnostics: diffy_core::pipeline::PipelineDiagnosticsInput {
            issue_patterns: input.issues,
            issues_state_path: input.issues_state,
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
    };

    let adapters = crate::cmd_common::default_pipeline_ingest_adapters();
    let output = diffy_core::pipeline::run_analysis(request, &adapters).map_err(|source| {
        context_kernel_core::KernelError::ReducerFailed {
            target: ctx.target.clone(),
            detail: source.to_string(),
        }
    })?;

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

    let refs = changed_paths
        .iter()
        .map(|path| {
            json!({
                "kind": "file",
                "value": path,
                "source": "diffy-analyze-v1",
                "relevance": 0.75
            })
        })
        .collect::<Vec<_>>();

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

    let packet_body = serde_json::to_value(json!({
        "packet_id": "diffy-analyze-v1",
        "tool": "diffy",
        "tools": ["diffy"],
        "reducer": "analyze",
        "reducers": ["analyze"],
        "paths": changed_paths,
        "payload": kernel_output,
        "sections": [
            {
                "id": "diff-gate-summary",
                "title": "Diff Gate Summary",
                "body": gate_summary,
                "refs": refs.clone(),
                "relevance": if output.gate_result.passed { 0.8 } else { 1.4 },
            },
            {
                "id": "changed-files",
                "title": "Changed Files",
                "body": changed_file_body,
                "refs": refs.clone(),
                "relevance": 0.9,
            }
        ],
        "refs": refs,
        "text_blobs": [gate_summary],
    }))
    .map_err(|source| context_kernel_core::KernelError::ReducerFailed {
        target: ctx.target.clone(),
        detail: source.to_string(),
    })?;

    Ok(context_kernel_core::ReducerResult {
        output_packets: vec![context_kernel_core::KernelPacket {
            packet_id: Some("diffy-analyze-v1".to_string()),
            format: "packet-json".to_string(),
            body: packet_body,
            token_usage: None,
            runtime_ms: None,
            metadata: json!({
                "reducer": "diffy.analyze",
                "passed": output.gate_result.passed,
            }),
        }],
        metadata: json!({
            "reducer": "diffy.analyze",
            "passed": output.gate_result.passed,
        }),
    })
}
