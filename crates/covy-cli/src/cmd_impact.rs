use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use covy_core::CovyConfig;
use std::path::Path;
use std::process::Command;

#[derive(Args)]
pub struct ImpactArgs {
    #[command(subcommand)]
    pub command: Option<ImpactCommand>,

    #[command(flatten)]
    pub legacy: LegacyImpactArgs,
}

#[derive(Subcommand)]
pub enum ImpactCommand {
    /// Build or update per-test impact map
    Record(ImpactRecordArgs),
    /// Plan tests for a git diff
    Plan(ImpactPlanArgs),
    /// Execute a previously generated impact plan
    Run(ImpactRunArgs),
}

#[derive(Args, Default)]
pub struct LegacyImpactArgs {
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

#[derive(Args, Default)]
pub struct ImpactRecordArgs {
    /// Base ref used for metadata tagging (default: main)
    #[arg(long, default_value = "main")]
    pub base_ref: String,

    /// Output testmap path
    #[arg(
        long = "output",
        default_value = ".covy/state/testmap.bin",
        alias = "out"
    )]
    pub output: String,

    /// Directory containing per-test LCOV reports
    #[arg(long)]
    pub per_test_lcov_dir: Option<String>,

    /// Directory containing per-test JaCoCo reports
    #[arg(long)]
    pub per_test_jacoco_dir: Option<String>,

    /// Directory containing per-test Cobertura reports
    #[arg(long)]
    pub per_test_cobertura_dir: Option<String>,

    /// JSONL manifest with test_id + coverage_report(s)
    #[arg(long)]
    pub test_report: Option<String>,

    /// Optional summary json output path
    #[arg(long)]
    pub summary_json: Option<String>,

    /// Print input schema/example and exit
    #[arg(long)]
    pub schema: bool,
}

#[derive(Args, Default)]
pub struct ImpactPlanArgs {
    /// Base ref for diff
    #[arg(long, default_value = "origin/main")]
    pub base_ref: String,

    /// Head ref for diff
    #[arg(long, default_value = "HEAD")]
    pub head_ref: String,

    /// Path to testmap state
    #[arg(long, default_value = ".covy/state/testmap.bin")]
    pub testmap: String,

    /// Maximum number of tests to select
    #[arg(long)]
    pub max_tests: Option<usize>,

    /// Target changed-lines coverage as a ratio in [0,1]
    #[arg(long)]
    pub target_coverage: Option<f64>,

    /// Output format (json only for now)
    #[arg(long, default_value = "json")]
    pub format: String,
}

#[derive(Args, Default)]
pub struct ImpactRunArgs {
    /// Path to impact plan json
    #[arg(long)]
    pub plan: Option<String>,

    /// Print input schema/example and exit
    #[arg(long)]
    pub schema: bool,

    /// Command template to execute (provide after --)
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub command: Vec<String>,
}

pub fn run(args: ImpactArgs, config_path: &str) -> Result<i32> {
    match args.command {
        Some(ImpactCommand::Record(record)) => run_record(record),
        Some(ImpactCommand::Plan(plan)) => run_plan(plan, config_path),
        Some(ImpactCommand::Run(run)) => run_impact_run(run),
        None => {
            crate::cmd_common::maybe_warn_deprecated(
                "warning: `covy impact` legacy mode is deprecated; use `covy impact plan` and `covy impact run`.",
            );
            run_legacy(args.legacy, config_path)
        }
    }
}

fn run_record(args: ImpactRecordArgs) -> Result<i32> {
    crate::cmd_common::warn_if_legacy_flag_used("--out", "--output");
    if args.schema {
        println!("{IMPACT_RECORD_MANIFEST_EXAMPLE}");
        return Ok(0);
    }

    let adapters = crate::cmd_common::default_impact_adapters();
    let response = covy_core::impact_pipeline::run_impact(
        covy_core::impact_pipeline::ImpactRequest {
            mode: covy_core::impact_pipeline::ImpactMode::Record(
                covy_core::impact_pipeline::ImpactRecordRequest {
                    base_ref: args.base_ref,
                    output: args.output,
                    per_test_lcov_dir: args.per_test_lcov_dir,
                    per_test_jacoco_dir: args.per_test_jacoco_dir,
                    per_test_cobertura_dir: args.per_test_cobertura_dir,
                    test_report: args.test_report,
                    summary_json: args.summary_json,
                },
            ),
        },
        &adapters,
    )?;

    let summary = response
        .record_summary
        .ok_or_else(|| anyhow::anyhow!("impact record response missing summary"))?;
    println!(
        "Recorded testmap: tests={} files={} cells={} out={}",
        summary.tests_total, summary.files_total, summary.non_empty_cells, summary.output
    );
    Ok(0)
}

fn run_plan(args: ImpactPlanArgs, config_path: &str) -> Result<i32> {
    if !args.format.eq_ignore_ascii_case("json") {
        anyhow::bail!(
            "Unsupported --format '{}'; only 'json' is supported",
            args.format
        );
    }

    let config = CovyConfig::load(Path::new(config_path)).unwrap_or_default();
    let max_tests = args.max_tests.unwrap_or(config.impact.max_tests);
    let target_coverage = args
        .target_coverage
        .unwrap_or(config.impact.target_coverage);

    let adapters = crate::cmd_common::default_impact_adapters();
    let response = covy_core::impact_pipeline::run_impact(
        covy_core::impact_pipeline::ImpactRequest {
            mode: covy_core::impact_pipeline::ImpactMode::Plan(
                covy_core::impact_pipeline::ImpactPlanRequest {
                    base_ref: args.base_ref,
                    head_ref: args.head_ref,
                    testmap: args.testmap,
                    max_tests,
                    target_coverage,
                },
            ),
        },
        &adapters,
    )?;

    let plan = response
        .plan
        .ok_or_else(|| anyhow::anyhow!("impact plan response missing plan"))?;
    println!("{}", serde_json::to_string_pretty(&plan)?);
    Ok(0)
}

const IMPACT_PLAN_EXAMPLE: &str = r#"{
  "changed_lines_total": 42,
  "changed_lines_covered_by_plan": 30,
  "plan_coverage_pct": 0.71,
  "tests": [
    {"id": "com.foo.BarTest", "name": "com.foo.BarTest", "estimated_overlap_lines": 10, "marginal_gain_lines": 5}
  ],
  "uncovered_blocks": [
    {"file": "src/main/java/com/foo/Bar.java", "start_line": 101, "end_line": 104}
  ],
  "next_command": "covy impact run --plan plan.json -- <your-test-command-template>"
}"#;

const IMPACT_RECORD_MANIFEST_EXAMPLE: &str = r#"{
  "type": "impact-record-manifest-jsonl",
  "description": "One JSON object per line.",
  "example_line": {
    "test_id": "com.foo.BarTest",
    "language": "java",
    "coverage_report": "path/to/jacoco.xml",
    "coverage_reports": ["path/to/jacoco.xml", "path/to/extra.xml"]
  }
}"#;

fn run_legacy(args: LegacyImpactArgs, config_path: &str) -> Result<i32> {
    let config = CovyConfig::load(Path::new(config_path)).unwrap_or_default();
    let base = args
        .base
        .as_deref()
        .unwrap_or(&config.diff.base)
        .to_string();
    let head = args
        .head
        .as_deref()
        .unwrap_or(&config.diff.head)
        .to_string();
    let testmap = if args.testmap == ".covy/state/testmap.bin" {
        config.impact.testmap_path
    } else {
        args.testmap
    };

    let adapters = crate::cmd_common::default_impact_adapters();
    let response = covy_core::impact_pipeline::run_impact(
        covy_core::impact_pipeline::ImpactRequest {
            mode: covy_core::impact_pipeline::ImpactMode::LegacySelect(
                covy_core::impact_pipeline::ImpactLegacyRequest {
                    base_ref: base,
                    head_ref: head,
                    testmap,
                    fresh_hours: config.impact.fresh_hours,
                    full_suite_threshold: config.impact.full_suite_threshold,
                    fallback_mode: config.impact.fallback_mode,
                    smoke_always: config.impact.smoke.always,
                    smoke_stale_extra: config.impact.smoke.stale_extra,
                    include_print_command: args.print_command,
                },
            ),
        },
        &adapters,
    )?;

    let result = response
        .impact_result
        .ok_or_else(|| anyhow::anyhow!("impact legacy response missing result"))?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(0);
    }

    if result.selected_tests.is_empty() {
        println!("(no impacted tests)");
    } else {
        for test in &result.selected_tests {
            println!("{test}");
        }
    }
    println!(
        "summary: selected={} known={} missing={} confidence={:.2} stale={} escalate_full_suite={}",
        result.selected_tests.len(),
        response.known_tests.unwrap_or(0),
        result.missing_mappings.len(),
        result.confidence,
        result.stale,
        result.escalate_full_suite
    );

    if args.print_command {
        if let Some(command) = response.print_command {
            println!("{command}");
        }
    }

    Ok(0)
}

fn run_impact_run(args: ImpactRunArgs) -> Result<i32> {
    if args.schema {
        println!("{IMPACT_PLAN_EXAMPLE}");
        return Ok(0);
    }

    let plan_path = args.plan.as_deref().ok_or_else(|| {
        anyhow::anyhow!("Missing --plan. Use: covy impact run --plan plan.json -- <command>")
    })?;

    if args.command.is_empty() {
        anyhow::bail!(
            "No command template provided. Use: covy impact run --plan plan.json -- <command>"
        );
    }

    let content = std::fs::read_to_string(plan_path)
        .with_context(|| format!("Failed to read plan at {plan_path}"))?;
    let plan: covy_core::impact::ImpactPlan = crate::cmd_common::deserialize_json_with_example(
        &content,
        "ImpactPlan",
        IMPACT_PLAN_EXAMPLE,
    )?;

    let tests: Vec<String> = plan.tests.iter().map(|t| t.id.clone()).collect();
    if tests.is_empty() {
        println!("No tests selected in plan; skipping execution.");
        return Ok(0);
    }

    let final_command = build_run_command_args(&args.command, &tests);
    if final_command.is_empty() {
        anyhow::bail!("Resolved command is empty");
    }

    let executable = &final_command[0];
    let status = Command::new(executable)
        .args(&final_command[1..])
        .status()?;
    Ok(status.code().unwrap_or(1))
}

fn build_run_command_args(template: &[String], tests: &[String]) -> Vec<String> {
    let tests_joined = tests.join(" ");
    let tests_csv = tests.join(",");
    let mut expanded = Vec::new();
    let mut had_placeholder = false;

    for token in template {
        if token == "{tests}" {
            had_placeholder = true;
            expanded.extend(tests.iter().cloned());
            continue;
        }

        if token.contains("{tests}") || token.contains("{tests_csv}") {
            had_placeholder = true;
        }
        let replaced = token
            .replace("{tests_csv}", &tests_csv)
            .replace("{tests}", &tests_joined);
        expanded.push(replaced);
    }

    if !had_placeholder {
        expanded.extend(tests.iter().cloned());
    }

    expanded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_run_command_args_expands_placeholders() {
        let template = vec![
            "pytest".to_string(),
            "{tests}".to_string(),
            "--maxfail=1".to_string(),
            "--csv={tests_csv}".to_string(),
        ];
        let tests = vec!["a::one".to_string(), "b::two".to_string()];
        let cmd = build_run_command_args(&template, &tests);
        assert_eq!(
            cmd,
            vec![
                "pytest".to_string(),
                "a::one".to_string(),
                "b::two".to_string(),
                "--maxfail=1".to_string(),
                "--csv=a::one,b::two".to_string()
            ]
        );
    }

    #[test]
    fn test_build_run_command_args_appends_tests_when_no_placeholders() {
        let template = vec!["pytest".to_string(), "-q".to_string()];
        let tests = vec!["a::one".to_string(), "b::two".to_string()];
        let cmd = build_run_command_args(&template, &tests);
        assert_eq!(
            cmd,
            vec![
                "pytest".to_string(),
                "-q".to_string(),
                "a::one".to_string(),
                "b::two".to_string()
            ]
        );
    }

    #[test]
    fn test_run_impact_run_skips_execution_for_empty_plan() {
        let dir = tempfile::TempDir::new().unwrap();
        let plan_path = dir.path().join("plan.json");
        let plan = covy_core::impact::ImpactPlan::default();
        std::fs::write(&plan_path, serde_json::to_string(&plan).unwrap()).unwrap();

        let result = run_impact_run(ImpactRunArgs {
            plan: Some(plan_path.to_string_lossy().to_string()),
            schema: false,
            command: vec!["definitely-not-a-command".to_string()],
        })
        .unwrap();
        assert_eq!(result, 0);
    }

    #[test]
    fn test_run_impact_run_executes_command() {
        let dir = tempfile::TempDir::new().unwrap();
        let plan_path = dir.path().join("plan.json");
        let plan = covy_core::impact::ImpactPlan {
            tests: vec![covy_core::impact::PlannedTest {
                id: "com.foo.BarTest".to_string(),
                name: "com.foo.BarTest".to_string(),
                estimated_overlap_lines: 1,
                marginal_gain_lines: 1,
            }],
            ..Default::default()
        };
        std::fs::write(&plan_path, serde_json::to_string(&plan).unwrap()).unwrap();

        let code = run_impact_run(ImpactRunArgs {
            plan: Some(plan_path.to_string_lossy().to_string()),
            schema: false,
            command: vec!["true".to_string(), "{tests}".to_string()],
        })
        .unwrap();
        assert_eq!(code, 0);
    }
}
