use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use covy_core::CovyConfig;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

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
    #[arg(long, default_value = ".covy/state/testmap.bin")]
    pub out: String,

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
    pub plan: String,

    /// Command template to execute (provide after --)
    #[arg(last = true, trailing_var_arg = true, allow_hyphen_values = true)]
    pub command: Vec<String>,
}

pub fn run(args: ImpactArgs, config_path: &str) -> Result<i32> {
    match args.command {
        Some(ImpactCommand::Record(record)) => run_record(record),
        Some(ImpactCommand::Plan(plan)) => run_plan(plan, config_path),
        Some(ImpactCommand::Run(run)) => run_impact_run(run),
        None => {
            eprintln!(
                "warning: `covy impact` legacy mode is deprecated; use `covy impact plan` and `covy impact run`."
            );
            run_legacy(args.legacy, config_path)
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum InputFormat {
    Auto,
    Lcov,
    JaCoCo,
    Cobertura,
}

#[derive(Debug, Clone)]
struct ReportSpec {
    path: PathBuf,
    format: InputFormat,
}

#[derive(Debug, Clone, Default)]
struct TestCoverageInput {
    id: String,
    language: Option<String>,
    reports: Vec<ReportSpec>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct ManifestRecord {
    test_id: String,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    coverage_report: Option<String>,
    #[serde(default)]
    coverage_reports: Vec<String>,
}

impl ManifestRecord {
    fn coverage_paths(&self) -> Vec<&str> {
        let mut paths = Vec::new();
        if let Some(path) = self.coverage_report.as_deref() {
            paths.push(path);
        }
        for path in &self.coverage_reports {
            paths.push(path.as_str());
        }
        paths
    }
}

#[derive(Debug, Clone, serde::Serialize)]
struct RecordSummary {
    tests_total: usize,
    files_total: usize,
    non_empty_cells: usize,
    output: String,
}

fn run_record(args: ImpactRecordArgs) -> Result<i32> {
    let mut by_test: BTreeMap<String, TestCoverageInput> = BTreeMap::new();

    if let Some(dir) = args.per_test_lcov_dir.as_deref() {
        collect_inputs_from_dir(dir, InputFormat::Lcov, &mut by_test)?;
    }
    if let Some(dir) = args.per_test_jacoco_dir.as_deref() {
        collect_inputs_from_dir(dir, InputFormat::JaCoCo, &mut by_test)?;
    }
    if let Some(dir) = args.per_test_cobertura_dir.as_deref() {
        collect_inputs_from_dir(dir, InputFormat::Cobertura, &mut by_test)?;
    }
    if let Some(path) = args.test_report.as_deref() {
        collect_inputs_from_manifest(path, &mut by_test)?;
    }

    if by_test.is_empty() {
        anyhow::bail!(
            "No per-test coverage inputs found. Provide at least one of --per-test-*-dir or --test-report."
        );
    }

    let (index, mut summary) = build_testmap_index(by_test, &args.base_ref)?;
    summary.output = args.out.clone();

    let output = Path::new(&args.out);
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = covy_core::cache::serialize_testmap(&index)?;
    std::fs::write(output, bytes)?;

    if let Some(summary_path) = args.summary_json.as_deref() {
        let summary_path = Path::new(summary_path);
        if let Some(parent) = summary_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&summary)?;
        std::fs::write(summary_path, json)?;
    }

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
        .unwrap_or(config.impact.target_coverage)
        .clamp(0.0, 1.0);

    let bytes = std::fs::read(&args.testmap)
        .with_context(|| format!("Failed to read testmap at {}", args.testmap))?;
    let map = covy_core::cache::deserialize_testmap(&bytes)?;
    if map.coverage.is_empty() || map.file_index.is_empty() || map.tests.is_empty() {
        anyhow::bail!(
            "Testmap '{}' does not include line-level v2 coverage data. Rebuild with `covy impact record`.",
            args.testmap
        );
    }

    let diffs = covy_core::diff::git_diff(&args.base_ref, &args.head_ref)?;
    let plan = covy_core::impact::plan_impacted_tests(&map, &diffs, max_tests, target_coverage);
    println!("{}", serde_json::to_string_pretty(&plan)?);
    Ok(0)
}

fn run_impact_run(args: ImpactRunArgs) -> Result<i32> {
    if args.command.is_empty() {
        anyhow::bail!(
            "No command template provided. Use: covy impact run --plan plan.json -- <command>"
        );
    }

    let content = std::fs::read_to_string(&args.plan)
        .with_context(|| format!("Failed to read plan at {}", args.plan))?;
    let plan: covy_core::impact::ImpactPlan =
        serde_json::from_str(&content).with_context(|| format!("Failed to parse {}", args.plan))?;

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

fn collect_inputs_from_dir(
    dir: &str,
    format: InputFormat,
    by_test: &mut BTreeMap<String, TestCoverageInput>,
) -> Result<()> {
    let dir_path = Path::new(dir);
    if !dir_path.exists() {
        anyhow::bail!("Coverage directory does not exist: {}", dir_path.display());
    }
    for entry in std::fs::read_dir(dir_path)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let test_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Cannot infer test id from {}", path.display()))?
            .to_string();

        let language = infer_language_from_test_id(&test_id);
        let input = by_test
            .entry(test_id.clone())
            .or_insert_with(|| TestCoverageInput {
                id: test_id.clone(),
                language: Some(language.clone()),
                reports: Vec::new(),
            });
        if input.language.is_none() {
            input.language = Some(language);
        }
        input.reports.push(ReportSpec { path, format });
    }
    Ok(())
}

fn collect_inputs_from_manifest(
    path: &str,
    by_test: &mut BTreeMap<String, TestCoverageInput>,
) -> Result<()> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read test report manifest {}", path))?;
    for (idx, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let rec: ManifestRecord = serde_json::from_str(line).with_context(|| {
            format!(
                "Invalid JSON in test report manifest {} at line {}",
                path,
                idx + 1
            )
        })?;
        if rec.test_id.trim().is_empty() {
            anyhow::bail!("Manifest {} line {} has empty test_id", path, idx + 1);
        }
        if rec.coverage_paths().is_empty() {
            anyhow::bail!(
                "Manifest {} line {} has no coverage_report(s) for test '{}'",
                path,
                idx + 1,
                rec.test_id
            );
        }

        let input = by_test
            .entry(rec.test_id.clone())
            .or_insert_with(|| TestCoverageInput {
                id: rec.test_id.clone(),
                language: rec.language.clone(),
                reports: Vec::new(),
            });
        if input.language.is_none() {
            input.language = rec.language.clone();
        }
        for coverage_path in rec.coverage_paths() {
            input.reports.push(ReportSpec {
                path: PathBuf::from(coverage_path),
                format: InputFormat::Auto,
            });
        }
    }
    Ok(())
}

fn build_testmap_index(
    by_test: BTreeMap<String, TestCoverageInput>,
    base_ref: &str,
) -> Result<(covy_core::testmap::TestMapIndex, RecordSummary)> {
    let mut index = covy_core::testmap::TestMapIndex::default();
    index.metadata.schema_version = covy_core::cache::TESTMAP_SCHEMA_VERSION;
    index.metadata.path_norm_version = covy_core::cache::DIAGNOSTICS_PATH_NORM_VERSION;
    index.metadata.repo_root_id = covy_core::cache::current_repo_root_id(None);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    index.metadata.generated_at = now;
    index.metadata.created_at = Some(now);
    index.metadata.granularity = "line".to_string();
    index.metadata.commit_sha = resolve_commit_sha(base_ref);

    let mut per_test_lines: BTreeMap<String, BTreeMap<String, Vec<u32>>> = BTreeMap::new();
    let mut file_index_set: BTreeSet<String> = BTreeSet::new();

    for (test_id, input) in by_test {
        let canonical_test_id = if input.id.trim().is_empty() {
            test_id.clone()
        } else {
            input.id.clone()
        };
        if input.reports.is_empty() {
            continue;
        }
        let mut combined = covy_core::CoverageData::new();
        for report in &input.reports {
            let data = match report.format {
                InputFormat::Auto => covy_ingest::ingest_path(&report.path),
                InputFormat::Lcov => covy_ingest::ingest_path_with_format(
                    &report.path,
                    covy_core::CoverageFormat::Lcov,
                ),
                InputFormat::JaCoCo => covy_ingest::ingest_path_with_format(
                    &report.path,
                    covy_core::CoverageFormat::JaCoCo,
                ),
                InputFormat::Cobertura => covy_ingest::ingest_path_with_format(
                    &report.path,
                    covy_core::CoverageFormat::Cobertura,
                ),
            }
            .with_context(|| {
                format!(
                    "Failed to parse coverage report '{}' for test '{}'",
                    report.path.display(),
                    canonical_test_id
                )
            })?;
            combined.merge(&data);
        }

        covy_core::pathmap::auto_normalize_paths(&mut combined, None);
        let mut line_map: BTreeMap<String, Vec<u32>> = BTreeMap::new();

        for (file, fc) in &combined.files {
            file_index_set.insert(file.clone());
            let lines: Vec<u32> = fc.lines_covered.iter().collect();
            line_map.insert(file.clone(), lines);
            index
                .test_to_files
                .entry(canonical_test_id.clone())
                .or_default()
                .insert(file.clone());
            index
                .file_to_tests
                .entry(file.clone())
                .or_default()
                .insert(canonical_test_id.clone());
        }

        let language = input
            .language
            .clone()
            .unwrap_or_else(|| infer_language_from_test_id(&canonical_test_id));
        index
            .test_language
            .insert(canonical_test_id.clone(), normalize_language(&language));
        per_test_lines.insert(canonical_test_id, line_map);
    }

    index.tests = per_test_lines.keys().cloned().collect();
    index.file_index = file_index_set.into_iter().collect();

    let mut coverage = Vec::with_capacity(index.tests.len());
    let mut non_empty_cells = 0usize;
    for test_id in &index.tests {
        let mut row = Vec::with_capacity(index.file_index.len());
        let map = per_test_lines.get(test_id);
        for file in &index.file_index {
            let lines = map
                .and_then(|m| m.get(file))
                .cloned()
                .unwrap_or_else(Vec::new);
            if !lines.is_empty() {
                non_empty_cells += 1;
            }
            row.push(lines);
        }
        coverage.push(row);
    }
    index.coverage = coverage;

    let summary = RecordSummary {
        tests_total: index.tests.len(),
        files_total: index.file_index.len(),
        non_empty_cells,
        output: String::new(),
    };

    Ok((index, summary))
}

fn resolve_commit_sha(base_ref: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--verify", base_ref])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if sha.is_empty() {
        None
    } else {
        Some(sha)
    }
}

fn normalize_language(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "python" | "py" => "python".to_string(),
        "go" => "go".to_string(),
        "custom" => "custom".to_string(),
        _ => "java".to_string(),
    }
}

fn infer_language_from_test_id(test_id: &str) -> String {
    if test_id.contains("::") || test_id.ends_with(".py") {
        "python".to_string()
    } else {
        "java".to_string()
    }
}

fn run_legacy(args: LegacyImpactArgs, config_path: &str) -> Result<i32> {
    let config = CovyConfig::load(Path::new(config_path)).unwrap_or_default();
    let base = args.base.as_deref().unwrap_or(&config.diff.base);
    let head = args.head.as_deref().unwrap_or(&config.diff.head);
    let testmap_path = if args.testmap == ".covy/state/testmap.bin" {
        config.impact.testmap_path.as_str()
    } else {
        args.testmap.as_str()
    };

    let bytes = std::fs::read(Path::new(testmap_path)).map_err(|e| {
        anyhow::anyhow!(
            "Failed to read testmap at {}: {e}",
            Path::new(testmap_path).display()
        )
    })?;
    let map = covy_core::cache::deserialize_testmap(&bytes)?;
    let known_tests = map.test_to_files.len();

    let diffs = covy_core::diff::git_diff(base, head)?;
    let mut result = covy_core::impact::select_impacted_tests(&map, &diffs);
    let stale = is_stale(map.metadata.generated_at, config.impact.fresh_hours);
    apply_policy(&mut result, &diffs, &config, stale, known_tests)?;

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
        known_tests,
        result.missing_mappings.len(),
        result.confidence,
        result.stale,
        result.escalate_full_suite
    );

    if args.print_command {
        let command = build_print_command(&result.selected_tests, &map.test_language);
        println!("{command}");
    }

    Ok(0)
}

fn is_stale(generated_at: u64, fresh_hours: u32) -> bool {
    if generated_at == 0 {
        return true;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let max_age = fresh_hours as u64 * 3600;
    now.saturating_sub(generated_at) > max_age
}

fn apply_policy(
    result: &mut covy_core::impact::ImpactResult,
    diffs: &[covy_core::model::FileDiff],
    config: &CovyConfig,
    stale: bool,
    known_tests: usize,
) -> Result<()> {
    result.stale = stale;

    let mut smoke: BTreeSet<String> = config.impact.smoke.always.iter().cloned().collect();
    if stale {
        smoke.extend(config.impact.smoke.stale_extra.iter().cloned());
    }

    let mut selected: BTreeSet<String> = result.selected_tests.iter().cloned().collect();
    selected.extend(smoke.iter().cloned());
    result.selected_tests = selected.into_iter().collect();
    result.smoke_tests = smoke.into_iter().collect();

    let total_changed = diffs.len();
    let missing = result.missing_mappings.len().min(total_changed);
    let mapped = total_changed.saturating_sub(missing);
    let mut confidence = if total_changed == 0 {
        1.0
    } else {
        mapped as f64 / total_changed as f64
    };
    if stale {
        confidence *= 0.75;
    }
    result.confidence = confidence.clamp(0.0, 1.0);
    if known_tests > 0 {
        let ratio = result.selected_tests.len() as f64 / known_tests as f64;
        result.escalate_full_suite = ratio > config.impact.full_suite_threshold;
    } else {
        result.escalate_full_suite = false;
    }

    if config
        .impact
        .fallback_mode
        .eq_ignore_ascii_case("fail-closed")
        && !result.missing_mappings.is_empty()
    {
        anyhow::bail!(
            "Impact mapping missing for {} changed file(s) in fail-closed mode",
            result.missing_mappings.len()
        );
    }

    Ok(())
}

fn build_print_command(
    selected_tests: &[String],
    test_language: &std::collections::BTreeMap<String, String>,
) -> String {
    if selected_tests.is_empty() {
        return "echo \"no impacted tests\"".to_string();
    }

    let mut java_tests = Vec::new();
    let mut python_tests = Vec::new();
    for test in selected_tests {
        let language = test_language
            .get(test)
            .map(|s| s.as_str())
            .unwrap_or_else(|| {
                if test.contains("::") {
                    "python"
                } else {
                    "java"
                }
            });
        if language.eq_ignore_ascii_case("python") {
            python_tests.push(test.clone());
        } else {
            java_tests.push(test.clone());
        }
    }

    let mut parts = Vec::new();
    if !java_tests.is_empty() {
        parts.push(format!("mvn -Dtest={} test", java_tests.join(",")));
    }
    if !python_tests.is_empty() {
        parts.push(format!("pytest {}", python_tests.join(" ")));
    }
    parts.join(" && ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_is_stale_with_zero_timestamp() {
        assert!(is_stale(0, 24));
    }

    #[test]
    fn test_apply_policy_adds_smoke_and_sets_confidence() {
        let mut cfg = CovyConfig::default();
        cfg.impact.smoke.always = vec!["smoke::always".to_string()];
        cfg.impact.smoke.stale_extra = vec!["smoke::stale".to_string()];

        let mut result = covy_core::impact::ImpactResult {
            selected_tests: vec!["t1".to_string()],
            smoke_tests: vec![],
            missing_mappings: vec!["src/a.rs".to_string()],
            stale: false,
            confidence: 1.0,
            escalate_full_suite: false,
        };
        let diffs = covy_core::diff::parse_diff_output(
            "diff --git a/src/a.rs b/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n-old\n+new\n",
        )
        .unwrap();

        apply_policy(&mut result, &diffs, &cfg, true, 4).unwrap();
        assert!(result.selected_tests.contains(&"t1".to_string()));
        assert!(result.selected_tests.contains(&"smoke::always".to_string()));
        assert!(result.selected_tests.contains(&"smoke::stale".to_string()));
        assert!(result.stale);
        assert!((result.confidence - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_apply_policy_sets_escalation_threshold() {
        let mut cfg = CovyConfig::default();
        cfg.impact.full_suite_threshold = 0.40;
        let mut result = covy_core::impact::ImpactResult {
            selected_tests: vec!["a".to_string(), "b".to_string(), "c".to_string()],
            ..Default::default()
        };
        let diffs = covy_core::diff::parse_diff_output(
            "diff --git a/src/a.rs b/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n-old\n+new\n",
        )
        .unwrap();
        apply_policy(&mut result, &diffs, &cfg, false, 5).unwrap();
        assert!(result.escalate_full_suite);
    }

    #[test]
    fn test_build_print_command() {
        let langs = std::collections::BTreeMap::new();
        let cmd = build_print_command(&["a.Test".to_string(), "b.Test".to_string()], &langs);
        assert_eq!(cmd, "mvn -Dtest=a.Test,b.Test test");
        assert_eq!(
            build_print_command(&[], &langs),
            "echo \"no impacted tests\""
        );
    }

    #[test]
    fn test_build_print_command_python_nodeids() {
        let mut langs = std::collections::BTreeMap::new();
        langs.insert(
            "tests/test_a.py::test_one".to_string(),
            "python".to_string(),
        );
        langs.insert(
            "tests/test_b.py::test_two".to_string(),
            "python".to_string(),
        );
        let cmd = build_print_command(
            &[
                "tests/test_a.py::test_one".to_string(),
                "tests/test_b.py::test_two".to_string(),
            ],
            &langs,
        );
        assert_eq!(
            cmd,
            "pytest tests/test_a.py::test_one tests/test_b.py::test_two"
        );
    }

    #[test]
    fn test_build_print_command_mixed_languages() {
        let mut langs = std::collections::BTreeMap::new();
        langs.insert("com.foo.BarTest".to_string(), "java".to_string());
        langs.insert(
            "tests/test_a.py::test_one".to_string(),
            "python".to_string(),
        );
        let cmd = build_print_command(
            &[
                "com.foo.BarTest".to_string(),
                "tests/test_a.py::test_one".to_string(),
            ],
            &langs,
        );
        assert_eq!(
            cmd,
            "mvn -Dtest=com.foo.BarTest test && pytest tests/test_a.py::test_one"
        );
    }

    fn fixture(rel: &str) -> PathBuf {
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        workspace.join("tests").join("fixtures").join(rel)
    }

    #[test]
    fn test_build_testmap_index_populates_v2_fields() {
        let mut by_test = BTreeMap::new();
        by_test.insert(
            "com.foo.BarTest".to_string(),
            TestCoverageInput {
                id: "com.foo.BarTest".to_string(),
                language: Some("java".to_string()),
                reports: vec![ReportSpec {
                    path: fixture("lcov/basic.info"),
                    format: InputFormat::Auto,
                }],
            },
        );

        let (index, summary) = build_testmap_index(by_test, "HEAD").unwrap();
        assert_eq!(summary.tests_total, 1);
        assert_eq!(index.tests, vec!["com.foo.BarTest".to_string()]);
        assert!(!index.file_index.is_empty());
        assert_eq!(index.coverage.len(), 1);
        assert_eq!(index.coverage[0].len(), index.file_index.len());
        assert_eq!(
            index
                .test_to_files
                .get("com.foo.BarTest")
                .map(|s| s.len())
                .unwrap_or_default(),
            index.file_index.len()
        );
    }

    #[test]
    fn test_collect_inputs_from_manifest_rejects_empty_test_id() {
        let dir = tempfile::TempDir::new().unwrap();
        let manifest = dir.path().join("manifest.jsonl");
        std::fs::write(
            &manifest,
            "{\"test_id\":\"\",\"coverage_report\":\"tests/fixtures/lcov/basic.info\"}\n",
        )
        .unwrap();

        let mut by_test = BTreeMap::new();
        let err =
            collect_inputs_from_manifest(manifest.to_str().unwrap(), &mut by_test).unwrap_err();
        assert!(err.to_string().contains("empty test_id"));
    }

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
            plan: plan_path.to_string_lossy().to_string(),
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
            plan: plan_path.to_string_lossy().to_string(),
            command: vec!["true".to_string(), "{tests}".to_string()],
        })
        .unwrap();
        assert_eq!(code, 0);
    }
}
