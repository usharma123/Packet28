use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};
use suite_packet_core::gate::{ImpactPlan, ImpactResult};

pub type ImpactError = anyhow::Error;

#[derive(Debug, Clone)]
pub struct ImpactRequest {
    pub mode: ImpactMode,
}

#[derive(Debug, Clone)]
pub enum ImpactMode {
    Record(ImpactRecordRequest),
    Plan(ImpactPlanRequest),
    LegacySelect(ImpactLegacyRequest),
}

#[derive(Debug, Clone)]
pub struct ImpactRecordRequest {
    pub base_ref: String,
    pub output: String,
    pub per_test_lcov_dir: Option<String>,
    pub per_test_jacoco_dir: Option<String>,
    pub per_test_cobertura_dir: Option<String>,
    pub test_report: Option<String>,
    pub summary_json: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ImpactPlanRequest {
    pub base_ref: String,
    pub head_ref: String,
    pub testmap: String,
    pub max_tests: usize,
    pub target_coverage: f64,
}

#[derive(Debug, Clone)]
pub struct ImpactLegacyRequest {
    pub base_ref: String,
    pub head_ref: String,
    pub testmap: String,
    pub fresh_hours: u32,
    pub full_suite_threshold: f64,
    pub fallback_mode: String,
    pub smoke_always: Vec<String>,
    pub smoke_stale_extra: Vec<String>,
    pub include_print_command: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ImpactRecordSummary {
    pub tests_total: usize,
    pub files_total: usize,
    pub non_empty_cells: usize,
    pub output: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ImpactBudgetUsage {
    pub max_tests: usize,
    pub target_coverage: f64,
    pub selected_tests: usize,
    pub achieved_plan_coverage_pct: f64,
}

#[derive(Debug, Clone)]
pub struct ImpactResponse {
    pub selected_tests: Vec<String>,
    pub plan: Option<ImpactPlan>,
    pub confidence: Option<f64>,
    pub budget_usage: Option<ImpactBudgetUsage>,
    pub record_summary: Option<ImpactRecordSummary>,
    pub impact_result: Option<ImpactResult>,
    pub print_command: Option<String>,
    pub known_tests: Option<usize>,
}

#[derive(Clone, Copy)]
pub struct ImpactAdapters {
    pub ingest_coverage_auto: fn(&Path) -> Result<crate::model::CoverageData>,
    pub ingest_coverage_with_format:
        fn(&Path, crate::model::CoverageFormat) -> Result<crate::model::CoverageData>,
    pub git_diff: fn(&str, &str) -> Result<Vec<crate::model::FileDiff>>,
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

pub fn run_impact(
    req: ImpactRequest,
    adapters: &ImpactAdapters,
) -> Result<ImpactResponse, ImpactError> {
    match req.mode {
        ImpactMode::Record(record) => run_record(record, adapters),
        ImpactMode::Plan(plan) => run_plan(plan, adapters),
        ImpactMode::LegacySelect(legacy) => run_legacy_select(legacy, adapters),
    }
}

fn run_record(args: ImpactRecordRequest, adapters: &ImpactAdapters) -> Result<ImpactResponse> {
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

    let (index, mut summary) = build_testmap_index(by_test, &args.base_ref, adapters)?;
    summary.output = args.output.clone();

    let output = Path::new(&args.output);
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = crate::cache::serialize_testmap(&index)?;
    std::fs::write(output, bytes)?;

    if let Some(summary_path) = args.summary_json.as_deref() {
        let summary_path = Path::new(summary_path);
        if let Some(parent) = summary_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&summary)?;
        std::fs::write(summary_path, json)?;
    }

    Ok(ImpactResponse {
        selected_tests: Vec::new(),
        plan: None,
        confidence: None,
        budget_usage: None,
        record_summary: Some(summary),
        impact_result: None,
        print_command: None,
        known_tests: None,
    })
}

fn run_plan(args: ImpactPlanRequest, adapters: &ImpactAdapters) -> Result<ImpactResponse> {
    let target_coverage = args.target_coverage.clamp(0.0, 1.0);

    let bytes = std::fs::read(&args.testmap)
        .with_context(|| format!("Failed to read testmap at {}", args.testmap))?;
    let map = crate::cache::deserialize_testmap(&bytes)?;
    if map.coverage.is_empty() || map.file_index.is_empty() || map.tests.is_empty() {
        anyhow::bail!(
            "Testmap '{}' does not include line-level v2 coverage data. Rebuild with `covy impact record`.",
            args.testmap
        );
    }

    let diffs = (adapters.git_diff)(&args.base_ref, &args.head_ref)?;
    let plan = crate::impact::plan_impacted_tests(&map, &diffs, args.max_tests, target_coverage);

    Ok(ImpactResponse {
        selected_tests: plan.tests.iter().map(|t| t.id.clone()).collect(),
        confidence: None,
        budget_usage: Some(ImpactBudgetUsage {
            max_tests: args.max_tests,
            target_coverage,
            selected_tests: plan.tests.len(),
            achieved_plan_coverage_pct: plan.plan_coverage_pct,
        }),
        plan: Some(plan),
        record_summary: None,
        impact_result: None,
        print_command: None,
        known_tests: None,
    })
}

fn run_legacy_select(
    args: ImpactLegacyRequest,
    adapters: &ImpactAdapters,
) -> Result<ImpactResponse> {
    let bytes = std::fs::read(Path::new(&args.testmap)).map_err(|e| {
        anyhow::anyhow!(
            "Failed to read testmap at {}: {e}",
            Path::new(&args.testmap).display()
        )
    })?;
    let map = crate::cache::deserialize_testmap(&bytes)?;
    let known_tests = map.test_to_files.len();

    let diffs = (adapters.git_diff)(&args.base_ref, &args.head_ref)?;
    let mut result = crate::impact::select_impacted_tests(&map, &diffs);
    let stale = is_stale(map.metadata.generated_at, args.fresh_hours);
    apply_policy(
        &mut result,
        &diffs,
        stale,
        known_tests,
        args.full_suite_threshold,
        &args.fallback_mode,
        &args.smoke_always,
        &args.smoke_stale_extra,
    )?;

    let print_command = if args.include_print_command {
        Some(build_print_command(
            &result.selected_tests,
            &map.test_language,
        ))
    } else {
        None
    };

    Ok(ImpactResponse {
        selected_tests: result.selected_tests.clone(),
        confidence: Some(result.confidence),
        budget_usage: None,
        plan: None,
        record_summary: None,
        impact_result: Some(result),
        print_command,
        known_tests: Some(known_tests),
    })
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
        let rec: ManifestRecord = serde_json::from_str(line).map_err(|e| {
            anyhow::anyhow!(
                "Invalid JSON in test report manifest {} at line {}: {e}\n\nExpected JSONL shape (one per line):\n  {{\"test_id\": \"com.foo.BarTest\", \"coverage_report\": \"path/to/jacoco.xml\"}}",
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
    adapters: &ImpactAdapters,
) -> Result<(crate::testmap::TestMapIndex, ImpactRecordSummary)> {
    let mut index = crate::testmap::TestMapIndex::default();
    index.metadata.schema_version = crate::cache::TESTMAP_SCHEMA_VERSION;
    index.metadata.path_norm_version = crate::cache::DIAGNOSTICS_PATH_NORM_VERSION;
    index.metadata.repo_root_id = crate::cache::current_repo_root_id(None);
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
        let mut combined = crate::model::CoverageData::new();
        for report in &input.reports {
            let data = match report.format {
                InputFormat::Auto => (adapters.ingest_coverage_auto)(&report.path),
                InputFormat::Lcov => (adapters.ingest_coverage_with_format)(
                    &report.path,
                    crate::model::CoverageFormat::Lcov,
                ),
                InputFormat::JaCoCo => (adapters.ingest_coverage_with_format)(
                    &report.path,
                    crate::model::CoverageFormat::JaCoCo,
                ),
                InputFormat::Cobertura => (adapters.ingest_coverage_with_format)(
                    &report.path,
                    crate::model::CoverageFormat::Cobertura,
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

        suite_foundation_core::pathmap::auto_normalize_paths(&mut combined, None);
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

    let summary = ImpactRecordSummary {
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
    result: &mut ImpactResult,
    diffs: &[crate::model::FileDiff],
    stale: bool,
    known_tests: usize,
    full_suite_threshold: f64,
    fallback_mode: &str,
    smoke_always: &[String],
    smoke_stale_extra: &[String],
) -> Result<()> {
    result.stale = stale;

    let mut smoke: BTreeSet<String> = smoke_always.iter().cloned().collect();
    if stale {
        smoke.extend(smoke_stale_extra.iter().cloned());
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
        result.escalate_full_suite = ratio > full_suite_threshold;
    } else {
        result.escalate_full_suite = false;
    }

    if fallback_mode.eq_ignore_ascii_case("fail-closed") && !result.missing_mappings.is_empty() {
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
    use roaring::RoaringBitmap;
    use std::collections::BTreeMap;

    fn fixture(rel: &str) -> PathBuf {
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        workspace.join("tests").join("fixtures").join(rel)
    }

    fn fake_coverage_for_path(path: &Path) -> Result<crate::model::CoverageData> {
        let mut data = crate::model::CoverageData::new();
        let mut fc = crate::model::FileCoverage::new();
        fc.lines_instrumented.insert(1);
        fc.lines_covered.insert(1);
        let key = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("file")
            .replace(".info", "")
            .replace(".xml", "");
        data.files.insert(format!("src/{key}.rs"), fc);
        Ok(data)
    }

    fn fake_coverage_with_format(
        path: &Path,
        _format: crate::model::CoverageFormat,
    ) -> Result<crate::model::CoverageData> {
        fake_coverage_for_path(path)
    }

    fn empty_git_diff(_base: &str, _head: &str) -> Result<Vec<crate::model::FileDiff>> {
        Ok(Vec::new())
    }

    fn default_adapters() -> ImpactAdapters {
        ImpactAdapters {
            ingest_coverage_auto: fake_coverage_for_path,
            ingest_coverage_with_format: fake_coverage_with_format,
            git_diff: empty_git_diff,
        }
    }

    #[test]
    fn test_manifest_record_rejects_empty_test_id() {
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
    fn test_manifest_record_rejects_missing_coverage() {
        let dir = tempfile::TempDir::new().unwrap();
        let manifest = dir.path().join("manifest.jsonl");
        std::fs::write(&manifest, "{\"test_id\":\"com.foo.BarTest\"}\n").unwrap();

        let mut by_test = BTreeMap::new();
        let err =
            collect_inputs_from_manifest(manifest.to_str().unwrap(), &mut by_test).unwrap_err();
        assert!(err.to_string().contains("no coverage_report(s)"));
    }

    #[test]
    fn test_run_record_builds_v2_testmap_and_summary() {
        let dir = tempfile::TempDir::new().unwrap();
        let per_test_dir = dir.path().join("per-test-lcov");
        std::fs::create_dir_all(&per_test_dir).unwrap();
        std::fs::copy(
            fixture("lcov/basic.info"),
            per_test_dir.join("com.foo.BarTest.info"),
        )
        .unwrap();

        let testmap = dir.path().join("testmap.bin");
        let summary_path = dir.path().join("summary.json");

        let resp = run_impact(
            ImpactRequest {
                mode: ImpactMode::Record(ImpactRecordRequest {
                    base_ref: "HEAD".to_string(),
                    output: testmap.to_string_lossy().to_string(),
                    per_test_lcov_dir: Some(per_test_dir.to_string_lossy().to_string()),
                    per_test_jacoco_dir: None,
                    per_test_cobertura_dir: None,
                    test_report: None,
                    summary_json: Some(summary_path.to_string_lossy().to_string()),
                }),
            },
            &default_adapters(),
        )
        .unwrap();

        let summary = resp.record_summary.unwrap();
        assert_eq!(summary.tests_total, 1);
        assert!(testmap.exists());
        assert!(summary_path.exists());

        let bytes = std::fs::read(&testmap).unwrap();
        let map = crate::cache::deserialize_testmap(&bytes).unwrap();
        assert_eq!(map.tests.len(), 1);
        assert_eq!(map.coverage.len(), 1);
        assert!(!map.file_index.is_empty());
        assert!(map.metadata.generated_at > 0);
    }

    #[test]
    fn test_run_plan_clamps_budget_and_returns_plan() {
        let dir = tempfile::TempDir::new().unwrap();
        let testmap = dir.path().join("testmap.bin");

        let mut map = crate::testmap::TestMapIndex::default();
        map.tests = vec!["t1".to_string()];
        map.file_index = vec!["src/a.rs".to_string()];
        map.coverage = vec![vec![vec![1]]];
        map.test_to_files.insert(
            "t1".to_string(),
            ["src/a.rs".to_string()].into_iter().collect(),
        );
        map.file_to_tests.insert(
            "src/a.rs".to_string(),
            ["t1".to_string()].into_iter().collect(),
        );
        std::fs::write(&testmap, crate::cache::serialize_testmap(&map).unwrap()).unwrap();

        let adapters = ImpactAdapters {
            ingest_coverage_auto: fake_coverage_for_path,
            ingest_coverage_with_format: fake_coverage_with_format,
            git_diff: |_base, _head| {
                let mut lines = RoaringBitmap::new();
                lines.insert(1);
                Ok(vec![crate::model::FileDiff {
                    path: "src/a.rs".to_string(),
                    old_path: None,
                    status: crate::model::DiffStatus::Modified,
                    changed_lines: lines,
                }])
            },
        };

        let resp = run_impact(
            ImpactRequest {
                mode: ImpactMode::Plan(ImpactPlanRequest {
                    base_ref: "HEAD".to_string(),
                    head_ref: "HEAD".to_string(),
                    testmap: testmap.to_string_lossy().to_string(),
                    max_tests: 1,
                    target_coverage: 2.5,
                }),
            },
            &adapters,
        )
        .unwrap();

        let budget = resp.budget_usage.unwrap();
        assert_eq!(budget.max_tests, 1);
        assert_eq!(budget.target_coverage, 1.0);
        assert_eq!(budget.selected_tests, 1);
        assert!(resp.plan.is_some());
    }

    #[test]
    fn test_run_plan_rejects_non_v2_testmap() {
        let dir = tempfile::TempDir::new().unwrap();
        let testmap = dir.path().join("testmap.bin");

        let map = crate::testmap::TestMapIndex::default();
        std::fs::write(&testmap, crate::cache::serialize_testmap(&map).unwrap()).unwrap();

        let err = run_impact(
            ImpactRequest {
                mode: ImpactMode::Plan(ImpactPlanRequest {
                    base_ref: "HEAD".to_string(),
                    head_ref: "HEAD".to_string(),
                    testmap: testmap.to_string_lossy().to_string(),
                    max_tests: 10,
                    target_coverage: 0.9,
                }),
            },
            &default_adapters(),
        )
        .unwrap_err();

        assert!(err
            .to_string()
            .contains("does not include line-level v2 coverage data"));
    }

    #[test]
    fn test_run_legacy_policy_smoke_confidence_and_escalation() {
        let dir = tempfile::TempDir::new().unwrap();
        let testmap = dir.path().join("testmap.bin");

        let mut map = crate::testmap::TestMapIndex::default();
        map.metadata.generated_at = 0;
        map.test_to_files.insert(
            "t1".to_string(),
            ["src/a.rs".to_string()].into_iter().collect(),
        );
        map.file_to_tests.insert(
            "src/a.rs".to_string(),
            ["t1".to_string()].into_iter().collect(),
        );
        map.test_language
            .insert("t1".to_string(), "java".to_string());
        std::fs::write(&testmap, crate::cache::serialize_testmap(&map).unwrap()).unwrap();

        let adapters = ImpactAdapters {
            ingest_coverage_auto: fake_coverage_for_path,
            ingest_coverage_with_format: fake_coverage_with_format,
            git_diff: |_base, _head| {
                let mut lines = RoaringBitmap::new();
                lines.insert(1);
                Ok(vec![crate::model::FileDiff {
                    path: "src/a.rs".to_string(),
                    old_path: None,
                    status: crate::model::DiffStatus::Modified,
                    changed_lines: lines,
                }])
            },
        };

        let resp = run_impact(
            ImpactRequest {
                mode: ImpactMode::LegacySelect(ImpactLegacyRequest {
                    base_ref: "HEAD".to_string(),
                    head_ref: "HEAD".to_string(),
                    testmap: testmap.to_string_lossy().to_string(),
                    fresh_hours: 24,
                    full_suite_threshold: 0.40,
                    fallback_mode: "fail-open".to_string(),
                    smoke_always: vec!["smoke::always".to_string()],
                    smoke_stale_extra: vec!["smoke::stale".to_string()],
                    include_print_command: true,
                }),
            },
            &adapters,
        )
        .unwrap();

        let result = resp.impact_result.unwrap();
        assert!(result.selected_tests.contains(&"t1".to_string()));
        assert!(result.selected_tests.contains(&"smoke::always".to_string()));
        assert!(result.selected_tests.contains(&"smoke::stale".to_string()));
        assert!(result.stale);
        assert!((result.confidence - 0.75).abs() < f64::EPSILON);
        assert!(result.escalate_full_suite);
        assert!(resp.print_command.is_some());
    }

    #[test]
    fn test_run_legacy_fail_closed_errors_on_missing_mappings() {
        let dir = tempfile::TempDir::new().unwrap();
        let testmap = dir.path().join("testmap.bin");

        let map = crate::testmap::TestMapIndex::default();
        std::fs::write(&testmap, crate::cache::serialize_testmap(&map).unwrap()).unwrap();

        let adapters = ImpactAdapters {
            ingest_coverage_auto: fake_coverage_for_path,
            ingest_coverage_with_format: fake_coverage_with_format,
            git_diff: |_base, _head| {
                let mut lines = RoaringBitmap::new();
                lines.insert(1);
                Ok(vec![crate::model::FileDiff {
                    path: "src/missing.rs".to_string(),
                    old_path: None,
                    status: crate::model::DiffStatus::Modified,
                    changed_lines: lines,
                }])
            },
        };

        let err = run_impact(
            ImpactRequest {
                mode: ImpactMode::LegacySelect(ImpactLegacyRequest {
                    base_ref: "HEAD".to_string(),
                    head_ref: "HEAD".to_string(),
                    testmap: testmap.to_string_lossy().to_string(),
                    fresh_hours: 24,
                    full_suite_threshold: 0.40,
                    fallback_mode: "fail-closed".to_string(),
                    smoke_always: Vec::new(),
                    smoke_stale_extra: Vec::new(),
                    include_print_command: false,
                }),
            },
            &adapters,
        )
        .unwrap_err();

        assert!(err.to_string().contains("fail-closed mode"));
    }
}
