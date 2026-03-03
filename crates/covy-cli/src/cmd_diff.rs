use std::path::Path;

use anyhow::Result;
use clap::Args;
use suite_foundation_core::config::GateConfig;
use suite_foundation_core::CovyConfig;

#[derive(Args)]
pub struct DiffArgs {
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

pub fn run(args: DiffArgs, config_path: &str) -> Result<i32> {
    let config = CovyConfig::load(Path::new(config_path)).unwrap_or_default();
    let report =
        if crate::cmd_common::resolve_json_output(args.json, args.report.as_deref(), "--report")? {
            "json".to_string()
        } else {
            crate::cmd_common::resolve_report_format(args.report.as_deref())
        };

    let base = args.base.as_deref().unwrap_or(&config.diff.base);
    let head = args.head.as_deref().unwrap_or(&config.diff.head);

    let gate_config = GateConfig {
        fail_under_total: args.fail_under_total.or(config.gate.fail_under_total),
        fail_under_changed: args.fail_under_changed.or(config.gate.fail_under_changed),
        fail_under_new: args.fail_under_new.or(config.gate.fail_under_new),
        issues: config.gate.issues.clone(),
    };

    let request = diffy_core::pipeline::PipelineRequest {
        base: base.to_string(),
        head: head.to_string(),
        source_root: None,
        coverage: diffy_core::pipeline::PipelineCoverageInput {
            paths: args.coverage,
            format: None,
            stdin: false,
            input_state_path: args.input,
            default_input_state_path: Some(".covy/state/latest.bin".to_string()),
            strip_prefixes: Vec::new(),
            reject_paths_with_input: false,
            no_inputs_error: "No coverage data found. Run `covy ingest` first or use --coverage."
                .to_string(),
        },
        diagnostics: diffy_core::pipeline::PipelineDiagnosticsInput {
            issue_patterns: args.issues,
            issues_state_path: args.issues_state,
            no_issues_state: args.no_issues_state,
            default_issues_state_path: ".covy/state/issues.bin".to_string(),
        },
        gate: gate_config,
    };

    let adapters = crate::cmd_common::default_pipeline_ingest_adapters();
    let output = diffy_core::pipeline::run_analysis(request, &adapters)?;

    match report.as_str() {
        "json" => {
            let json = diffy_core::report::render_gate_json(&output.gate_result);
            println!("{json}");
        }
        _ => {
            diffy_core::report::render_gate_result(&output.gate_result);
            if let Some(diag) = output.diagnostics.as_ref() {
                diffy_core::report::render_issues_terminal(
                    diag,
                    Some(&output.changed_line_context.diffs),
                );
            }
        }
    }

    Ok(if output.gate_result.passed { 0 } else { 1 })
}
