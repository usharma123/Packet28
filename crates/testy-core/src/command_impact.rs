use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};
use suite_foundation_core::CovyConfig;

#[derive(Debug, Clone)]
pub struct LegacyImpactArgs {
    pub base: Option<String>,
    pub head: Option<String>,
    pub testmap: String,
    pub print_command: bool,
}

#[derive(Debug, Clone)]
pub struct ImpactRecordArgs {
    pub base_ref: String,
    pub output: String,
    pub per_test_lcov_dir: Option<String>,
    pub per_test_jacoco_dir: Option<String>,
    pub per_test_cobertura_dir: Option<String>,
    pub test_report: Option<String>,
    pub summary_json: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ImpactPlanArgs {
    pub base_ref: String,
    pub head_ref: String,
    pub testmap: String,
    pub max_tests: Option<usize>,
    pub target_coverage: Option<f64>,
    pub format: String,
}

#[derive(Debug, Clone)]
pub struct ImpactRunArgs {
    pub plan_path: String,
    pub command: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ImpactLegacyOutput {
    pub result: crate::impact::ImpactResult,
    pub known_tests: usize,
    pub print_command: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImpactRunOutcome {
    SkippedEmptyPlan,
    ExitCode(i32),
}

pub fn run_legacy_impact(
    args: LegacyImpactArgs,
    config_path: &str,
    adapters: &crate::pipeline::ImpactAdapters,
) -> Result<ImpactLegacyOutput> {
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

    let response = crate::pipeline::run_impact(
        crate::pipeline::ImpactRequest {
            mode: crate::pipeline::ImpactMode::LegacySelect(crate::pipeline::ImpactLegacyRequest {
                base_ref: base,
                head_ref: head,
                testmap,
                fresh_hours: config.impact.fresh_hours,
                full_suite_threshold: config.impact.full_suite_threshold,
                fallback_mode: config.impact.fallback_mode,
                smoke_always: config.impact.smoke.always,
                smoke_stale_extra: config.impact.smoke.stale_extra,
                include_print_command: args.print_command,
            }),
        },
        adapters,
    )?;

    Ok(ImpactLegacyOutput {
        result: response
            .impact_result
            .ok_or_else(|| anyhow::anyhow!("impact legacy response missing result"))?,
        known_tests: response.known_tests.unwrap_or(0),
        print_command: response.print_command,
    })
}

pub fn run_record(
    args: ImpactRecordArgs,
    adapters: &crate::pipeline::ImpactAdapters,
) -> Result<crate::pipeline::ImpactRecordSummary> {
    let response = crate::pipeline::run_impact(
        crate::pipeline::ImpactRequest {
            mode: crate::pipeline::ImpactMode::Record(crate::pipeline::ImpactRecordRequest {
                base_ref: args.base_ref,
                output: args.output,
                per_test_lcov_dir: args.per_test_lcov_dir,
                per_test_jacoco_dir: args.per_test_jacoco_dir,
                per_test_cobertura_dir: args.per_test_cobertura_dir,
                test_report: args.test_report,
                summary_json: args.summary_json,
            }),
        },
        adapters,
    )?;

    response
        .record_summary
        .ok_or_else(|| anyhow::anyhow!("impact record response missing summary"))
}

pub fn run_plan(
    args: ImpactPlanArgs,
    config_path: &str,
    adapters: &crate::pipeline::ImpactAdapters,
) -> Result<crate::impact::ImpactPlan> {
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

    let response = crate::pipeline::run_impact(
        crate::pipeline::ImpactRequest {
            mode: crate::pipeline::ImpactMode::Plan(crate::pipeline::ImpactPlanRequest {
                base_ref: args.base_ref,
                head_ref: args.head_ref,
                testmap: args.testmap,
                max_tests,
                target_coverage,
            }),
        },
        adapters,
    )?;

    response
        .plan
        .ok_or_else(|| anyhow::anyhow!("impact plan response missing plan"))
}

pub fn run_impact_run(args: ImpactRunArgs, binary_name: &str) -> Result<ImpactRunOutcome> {
    if args.command.is_empty() {
        anyhow::bail!(
            "No command template provided. Use: {binary_name} impact run --plan plan.json -- <command>"
        );
    }

    let content = std::fs::read_to_string(&args.plan_path)
        .with_context(|| format!("Failed to read plan at {}", args.plan_path))?;
    let plan: crate::impact::ImpactPlan =
        deserialize_json_with_example(&content, "ImpactPlan", &impact_plan_example(binary_name))?;

    let tests: Vec<String> = plan.tests.iter().map(|t| t.id.clone()).collect();
    if tests.is_empty() {
        return Ok(ImpactRunOutcome::SkippedEmptyPlan);
    }

    let final_command = build_run_command_args(&args.command, &tests);
    if final_command.is_empty() {
        anyhow::bail!("Resolved command is empty");
    }

    let executable = &final_command[0];
    let status = Command::new(executable)
        .args(&final_command[1..])
        .status()?;
    Ok(ImpactRunOutcome::ExitCode(status.code().unwrap_or(1)))
}

pub fn build_run_command_args(template: &[String], tests: &[String]) -> Vec<String> {
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

pub fn impact_plan_example(binary_name: &str) -> String {
    format!(
        r#"{{
  "changed_lines_total": 42,
  "changed_lines_covered_by_plan": 30,
  "plan_coverage_pct": 0.71,
  "tests": [
    {{"id": "com.foo.BarTest", "name": "com.foo.BarTest", "estimated_overlap_lines": 10, "marginal_gain_lines": 5}}
  ],
  "uncovered_blocks": [
    {{"file": "src/main/java/com/foo/Bar.java", "start_line": 101, "end_line": 104}}
  ],
  "next_command": "{binary_name} impact run --plan plan.json -- <your-test-command-template>"
}}"#
    )
}

pub const IMPACT_RECORD_MANIFEST_EXAMPLE: &str = r#"{
  "type": "impact-record-manifest-jsonl",
  "description": "One JSON object per line.",
  "example_line": {
    "test_id": "com.foo.BarTest",
    "language": "java",
    "coverage_report": "path/to/jacoco.xml",
    "coverage_reports": ["path/to/jacoco.xml", "path/to/extra.xml"]
  }
}"#;

fn deserialize_json_with_example<T: serde::de::DeserializeOwned>(
    input: &str,
    type_name: &str,
    example: &str,
) -> Result<T> {
    serde_json::from_str(input).map_err(|e| {
        anyhow::anyhow!("Failed to parse {type_name}: {e}\n\nExpected JSON shape:\n{example}")
    })
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
        let plan = crate::impact::ImpactPlan::default();
        std::fs::write(&plan_path, serde_json::to_string(&plan).unwrap()).unwrap();

        let result = run_impact_run(
            ImpactRunArgs {
                plan_path: plan_path.to_string_lossy().to_string(),
                command: vec!["definitely-not-a-command".to_string()],
            },
            "testy",
        )
        .unwrap();
        assert_eq!(result, ImpactRunOutcome::SkippedEmptyPlan);
    }

    #[test]
    fn test_run_impact_run_executes_command() {
        let dir = tempfile::TempDir::new().unwrap();
        let plan_path = dir.path().join("plan.json");
        let plan = crate::impact::ImpactPlan {
            tests: vec![crate::impact::PlannedTest {
                id: "com.foo.BarTest".to_string(),
                name: "com.foo.BarTest".to_string(),
                estimated_overlap_lines: 1,
                marginal_gain_lines: 1,
            }],
            ..Default::default()
        };
        std::fs::write(&plan_path, serde_json::to_string(&plan).unwrap()).unwrap();

        let code = run_impact_run(
            ImpactRunArgs {
                plan_path: plan_path.to_string_lossy().to_string(),
                command: vec!["true".to_string(), "{tests}".to_string()],
            },
            "testy",
        )
        .unwrap();
        assert_eq!(code, ImpactRunOutcome::ExitCode(0));
    }
}
