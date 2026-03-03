use std::path::Path;

use anyhow::{anyhow, Result};
use clap::Args;
use roaring::RoaringBitmap;
use serde::{Deserialize, Serialize};
use serde_json::json;
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

    /// Output format (terminal/json). Defaults to "terminal" in interactive
    /// mode and "json" when stdout is piped.
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

pub fn run(args: AnalyzeArgs, config_path: &str) -> Result<i32> {
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
        ..context_kernel_core::KernelRequest::default()
    })?;

    let output_packet = response
        .output_packets
        .first()
        .ok_or_else(|| anyhow!("kernel returned no output packets"))?;
    let output: DiffAnalyzeKernelOutput = serde_json::from_value(output_packet.body.clone())?;

    match report.as_str() {
        "json" => {
            let json = diffy_core::report::render_gate_json(&output.gate_result);
            println!("{json}");
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
        }
    }

    Ok(if output.gate_result.passed { 0 } else { 1 })
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

    let packet_body = serde_json::to_value(DiffAnalyzeKernelOutput {
        gate_result: output.gate_result.clone(),
        diagnostics: output.diagnostics.clone(),
        diffs: output
            .changed_line_context
            .diffs
            .iter()
            .map(SerializableFileDiff::from_file_diff)
            .collect(),
    })
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
