use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Args, ValueEnum};
use suite_foundation_core::config::{GateConfig, IssueGateConfig};
use suite_foundation_core::CovyConfig;
use suite_packet_core::{BudgetCost, CoverageFormat, EnvelopeV1, FileRef, Provenance};

#[derive(Debug, Clone, Copy, ValueEnum, Default, PartialEq, Eq)]
pub enum PacketDetailArg {
    #[default]
    Compact,
    Rich,
}

#[derive(Args)]
pub struct CheckArgs {
    /// Coverage report file paths (supports globs)
    #[arg(long = "coverage")]
    coverage: Vec<String>,

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

    /// Output format (terminal/json/markdown/github). Defaults to "terminal".
    #[arg(long)]
    report: Option<String>,

    /// Emit JSON output
    #[arg(long, value_enum, num_args = 0..=1, default_missing_value = "compact")]
    json: Option<crate::cmd_common::JsonProfileArg>,

    /// Packet detail level in JSON mode
    #[arg(long, value_enum, default_value_t = PacketDetailArg::Compact)]
    packet_detail: PacketDetailArg,

    /// Emit one-release compatibility JSON shape
    #[arg(long)]
    legacy_json: bool,

    /// Pretty-print JSON output
    #[arg(long)]
    pretty: bool,

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
    let machine_profile =
        crate::cmd_common::resolve_machine_profile(args.json, args.report.as_deref(), "--report")?;
    let report = if machine_profile.is_some() {
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

    let mut coverage_paths = args.coverage;
    coverage_paths.extend(args.paths);

    let request = diffy_core::pipeline::PipelineRequest {
        base: base.to_string(),
        head: head.to_string(),
        source_root,
        coverage: diffy_core::pipeline::PipelineCoverageInput {
            paths: coverage_paths,
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
            let gate_json = serde_json::to_value(&output.gate_result).unwrap_or_default();
            let gate_json_bytes = serde_json::to_vec(&gate_json).unwrap_or_default();

            let mut changed_paths = output
                .changed_line_context
                .changed_paths
                .iter()
                .cloned()
                .collect::<Vec<_>>();
            changed_paths.sort();

            let envelope = EnvelopeV1 {
                version: "1".to_string(),
                tool: "covy".to_string(),
                kind: "coverage_gate".to_string(),
                hash: String::new(),
                summary: format!(
                    "passed={} changed={:?} total={:?} new={:?}",
                    output.gate_result.passed,
                    output.gate_result.changed_coverage_pct,
                    output.gate_result.total_coverage_pct,
                    output.gate_result.new_file_coverage_pct
                ),
                files: changed_paths
                    .iter()
                    .map(|path| FileRef {
                        path: path.clone(),
                        relevance: Some(0.75),
                        source: Some("cover.check".to_string()),
                    })
                    .collect(),
                symbols: Vec::new(),
                risk: None,
                confidence: Some(if output.gate_result.passed { 1.0 } else { 0.8 }),
                budget_cost: BudgetCost {
                    est_tokens: 0,
                    est_bytes: 0,
                    runtime_ms: 0,
                    tool_calls: 1,
                    payload_est_tokens: Some((gate_json_bytes.len() / 4) as u64),
                    payload_est_bytes: Some(gate_json_bytes.len()),
                },
                provenance: Provenance {
                    inputs: changed_paths,
                    git_base: Some(base.to_string()),
                    git_head: Some(head.to_string()),
                    generated_at_unix: now_unix(),
                },
                payload: gate_json,
            }
            .with_canonical_hash_and_real_budget();

            if args.legacy_json {
                let value = if args.packet_detail == PacketDetailArg::Rich {
                    serde_json::json!({
                        "schema_version": "suite.cover.check.v1",
                        "gate_result": output.gate_result,
                        "envelope_v1": envelope,
                    })
                } else {
                    serde_json::json!({
                        "schema_version": "suite.cover.check.v1",
                        "packet": envelope,
                    })
                };
                crate::cmd_common::emit_json(&value, args.pretty)?;
                return Ok(if output.gate_result.passed { 0 } else { 1 });
            }

            let mut effective_profile =
                machine_profile.unwrap_or(suite_packet_core::JsonProfile::Compact);
            if effective_profile == suite_packet_core::JsonProfile::Compact
                && args.packet_detail == PacketDetailArg::Rich
            {
                effective_profile = suite_packet_core::JsonProfile::Full;
            }
            crate::cmd_common::emit_machine_envelope(
                suite_packet_core::PACKET_TYPE_COVER_CHECK,
                &envelope,
                effective_profile,
                args.pretty,
                &crate::cmd_common::resolve_artifact_root(None),
                None,
            )?;
        }
        "markdown" => {
            let md = diffy_core::report::render_markdown(
                &output.coverage,
                &output.gate_result,
                &output.changed_line_context.diffs,
                args.show_missing,
                output.diagnostics.as_ref(),
            );
            print!("{md}");
        }
        "github" => {
            diffy_core::report::render_github_annotations(
                &output.coverage,
                &output.changed_line_context.diffs,
                &output.gate_result,
                output.diagnostics.as_ref(),
            );
        }
        _ => {
            diffy_core::report::render_terminal(
                &output.coverage,
                args.show_missing,
                "name",
                None,
                false,
            );
            if let Some(diag) = output.diagnostics.as_ref() {
                diffy_core::report::render_issues_terminal(
                    diag,
                    Some(&output.changed_line_context.diffs),
                );
            }
            diffy_core::report::render_gate_result(&output.gate_result);
        }
    }

    Ok(if output.gate_result.passed { 0 } else { 1 })
}

pub fn run_remote(args: CheckArgs, config_path: &str, daemon_root: &Path) -> Result<i32> {
    let machine_profile =
        crate::cmd_common::resolve_machine_profile(args.json, args.report.as_deref(), "--report")?;
    if machine_profile.is_none() || args.legacy_json || args.stdin {
        return run(args, config_path);
    }

    let response = crate::cmd_daemon::send_cover_check(
        daemon_root,
        packet28_daemon_core::CoverCheckRequest {
            coverage: args.coverage,
            paths: args.paths,
            format: args.format,
            issues: args.issues,
            issues_state: args.issues_state,
            no_issues_state: args.no_issues_state,
            base: args.base,
            head: args.head,
            fail_under_total: args.fail_under_total,
            fail_under_changed: args.fail_under_changed,
            fail_under_new: args.fail_under_new,
            max_new_errors: args.max_new_errors,
            max_new_warnings: args.max_new_warnings,
            input: args.input,
            strip_prefix: args.strip_prefix,
            source_root: args.source_root,
            show_missing: args.show_missing,
            config_path: config_path.to_string(),
        },
    )?;

    let mut effective_profile =
        machine_profile.unwrap_or(suite_packet_core::JsonProfile::Compact);
    if effective_profile == suite_packet_core::JsonProfile::Compact
        && args.packet_detail == PacketDetailArg::Rich
    {
        effective_profile = suite_packet_core::JsonProfile::Full;
    }
    crate::cmd_common::emit_machine_envelope(
        &response.packet_type,
        &response.envelope,
        effective_profile,
        args.pretty,
        &crate::cmd_common::resolve_artifact_root(None),
        None,
    )?;
    Ok(response.exit_code)
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

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
