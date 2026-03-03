use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Args;
use covy_core::config::{GateConfig, IssueGateConfig};
use covy_core::model::CoverageFormat;
use covy_core::CovyConfig;

#[derive(Args)]
pub struct CheckArgs {
    /// Coverage report file paths (supports globs)
    #[arg()]
    paths: Vec<String>,

    /// Coverage format (auto/lcov/cobertura/jacoco/gocov/llvm-cov)
    #[arg(short, long, default_value = "auto")]
    format: String,

    /// SARIF diagnostics file paths (supports globs)
    #[arg(long)]
    issues: Vec<String>,

    /// Path to cached diagnostics state file (default: .covy/state/issues.bin)
    #[arg(long)]
    issues_state: Option<String>,

    /// Disable automatic diagnostics state loading when --issues is not provided
    #[arg(long)]
    no_issues_state: bool,

    /// Read coverage data from stdin
    #[arg(long)]
    stdin: bool,

    /// Base ref for diff (default: main)
    #[arg(long)]
    base: Option<String>,

    /// Head ref for diff (default: HEAD)
    #[arg(long)]
    head: Option<String>,

    /// Fail if total coverage is below this %
    #[arg(long)]
    fail_under_total: Option<f64>,

    /// Fail if changed lines coverage is below this %
    #[arg(long)]
    fail_under_changed: Option<f64>,

    /// Fail if new file coverage is below this %
    #[arg(long)]
    fail_under_new: Option<f64>,

    /// Fail if changed-line errors exceed this value
    #[arg(long)]
    max_new_errors: Option<u32>,

    /// Fail if changed-line warnings exceed this value
    #[arg(long)]
    max_new_warnings: Option<u32>,

    /// Output format (terminal/json/markdown/github). Defaults to "terminal"
    /// in interactive mode and "json" when stdout is piped.
    #[arg(long)]
    report: Option<String>,

    /// Emit JSON output
    #[arg(long)]
    json: bool,

    /// Prefixes to strip from file paths in coverage data
    #[arg(long)]
    strip_prefix: Vec<String>,

    /// Source root for resolving relative paths
    #[arg(long)]
    source_root: Option<String>,

    /// Path to coverage state file
    #[arg(long)]
    input: Option<String>,

    /// Show missing line numbers
    #[arg(long)]
    show_missing: bool,
}

pub fn run(args: CheckArgs, config_path: &str) -> Result<i32> {
    let config = CovyConfig::load(Path::new(config_path)).unwrap_or_default();
    let report =
        if crate::cmd_common::resolve_json_output(args.json, args.report.as_deref(), "--report")? {
            "json".to_string()
        } else {
            crate::cmd_common::resolve_report_format(args.report.as_deref())
        };

    let base = args.base.as_deref().unwrap_or(&config.diff.base);
    let head = args.head.as_deref().unwrap_or(&config.diff.head);

    let issue_gate = IssueGateConfig {
        max_new_errors: args.max_new_errors.or(config.gate.issues.max_new_errors),
        max_new_warnings: args
            .max_new_warnings
            .or(config.gate.issues.max_new_warnings),
        max_new_issues: config.gate.issues.max_new_issues,
    };

    let gate_config = GateConfig {
        fail_under_total: args.fail_under_total.or(config.gate.fail_under_total),
        fail_under_changed: args.fail_under_changed.or(config.gate.fail_under_changed),
        fail_under_new: args.fail_under_new.or(config.gate.fail_under_new),
        issues: issue_gate,
    };

    let coverage_format = parse_format(&args.format)?;
    let source_root = args.source_root.as_ref().map(PathBuf::from);
    let strip_prefixes: Vec<String> = args
        .strip_prefix
        .iter()
        .cloned()
        .chain(config.ingest.strip_prefixes.iter().cloned())
        .collect();

    let request = covy_core::pipeline::PipelineRequest {
        base: base.to_string(),
        head: head.to_string(),
        source_root,
        coverage: covy_core::pipeline::PipelineCoverageInput {
            paths: args.paths,
            format: coverage_format,
            stdin: args.stdin,
            input_state_path: args.input,
            default_input_state_path: Some(".covy/state/latest.bin".to_string()),
            strip_prefixes,
            reject_paths_with_input: true,
            no_inputs_error:
                "No coverage files specified. Provide file paths, use --stdin, or run `covy ingest` first."
                    .to_string(),
        },
        diagnostics: covy_core::pipeline::PipelineDiagnosticsInput {
            issue_patterns: args.issues,
            issues_state_path: args.issues_state,
            no_issues_state: args.no_issues_state,
            default_issues_state_path: ".covy/state/issues.bin".to_string(),
        },
        gate: gate_config,
    };

    let adapters = crate::cmd_common::default_pipeline_ingest_adapters();
    let output = covy_core::pipeline::run_pipeline(request, &adapters)?;

    match report.as_str() {
        "json" => {
            let json = covy_core::report::render_gate_json(&output.gate_result);
            println!("{json}");
        }
        "markdown" => {
            let md = covy_core::report::render_markdown(
                &output.coverage,
                &output.gate_result,
                &output.changed_line_context.diffs,
                args.show_missing,
                output.diagnostics.as_ref(),
            );
            print!("{md}");
        }
        "github" => {
            covy_core::report::render_github_annotations(
                &output.coverage,
                &output.changed_line_context.diffs,
                &output.gate_result,
                output.diagnostics.as_ref(),
            );
        }
        _ => {
            covy_core::report::render_terminal(
                &output.coverage,
                args.show_missing,
                "name",
                None,
                false,
            );
            if let Some(diag) = output.diagnostics.as_ref() {
                covy_core::report::render_issues_terminal(
                    diag,
                    Some(&output.changed_line_context.diffs),
                );
            }
            covy_core::report::render_gate_result(&output.gate_result);
        }
    }

    Ok(if output.gate_result.passed { 0 } else { 1 })
}

fn parse_format(s: &str) -> Result<Option<CoverageFormat>> {
    match s {
        "lcov" => Ok(Some(CoverageFormat::Lcov)),
        "cobertura" => Ok(Some(CoverageFormat::Cobertura)),
        "jacoco" => Ok(Some(CoverageFormat::JaCoCo)),
        "gocov" => Ok(Some(CoverageFormat::GoCov)),
        "llvm-cov" => Ok(Some(CoverageFormat::LlvmCov)),
        "auto" => Ok(None),
        other => anyhow::bail!("Unknown format: {other}"),
    }
}
