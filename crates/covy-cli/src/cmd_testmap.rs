use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use serde::Deserialize;

#[derive(Args)]
pub struct TestmapArgs {
    #[command(subcommand)]
    pub command: TestmapCommands,
}

#[derive(Subcommand)]
pub enum TestmapCommands {
    /// Build test impact map artifacts
    Build(TestmapBuildArgs),
}

#[derive(Args)]
pub struct TestmapBuildArgs {
    /// Input manifest glob(s)
    #[arg(long)]
    pub manifest: Vec<String>,

    /// Output test map path
    #[arg(long, default_value = ".covy/state/testmap.bin")]
    pub output: String,

    /// Output timing map path
    #[arg(long, default_value = ".covy/state/testtimings.bin")]
    pub timings_output: String,
}

pub fn run(args: TestmapArgs, _config_path: &str) -> Result<i32> {
    match args.command {
        TestmapCommands::Build(build) => {
            let files = resolve_globs(&build.manifest)?;
            if files.is_empty() {
                anyhow::bail!("No manifest files found");
            }
            let records = read_manifest_records(&files)?;
            validate_manifest_records(&records)?;
            let mut index = covy_core::testmap::TestMapIndex::default();
            index.test_language = build_test_language_index(&records)?;
            index.test_to_files = build_test_to_files_index(&records)?;
            index.file_to_tests = build_file_to_tests_index(&index.test_to_files);
            index.metadata.schema_version = covy_core::cache::TESTMAP_SCHEMA_VERSION;
            index.metadata.path_norm_version = covy_core::cache::DIAGNOSTICS_PATH_NORM_VERSION;
            index.metadata.repo_root_id = covy_core::cache::current_repo_root_id(None);
            index.metadata.generated_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            index.metadata.granularity = "file".to_string();

            let output = Path::new(&build.output);
            if let Some(parent) = output.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let bytes = covy_core::cache::serialize_testmap(&index)?;
            std::fs::write(output, bytes)?;

            tracing::info!(
                "Built testmap from {} manifest records across {} file(s)",
                records.len(),
                files.len()
            );
            Ok(0)
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ManifestRecord {
    test_id: String,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    duration_ms: Option<u64>,
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

fn resolve_globs(patterns: &[String]) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for pattern in patterns {
        let matches: Vec<_> = glob::glob(pattern)
            .context(format!("Invalid glob pattern: {pattern}"))?
            .filter_map(|r| r.ok())
            .collect();
        if matches.is_empty() {
            tracing::warn!("No files matched pattern: {}", pattern);
        }
        files.extend(matches);
    }
    Ok(files)
}

fn read_manifest_records(files: &[PathBuf]) -> Result<Vec<ManifestRecord>> {
    let mut out = Vec::new();
    for file in files {
        let content = std::fs::read_to_string(file)
            .with_context(|| format!("Failed to read manifest file {}", file.display()))?;
        for (idx, line) in content.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let rec: ManifestRecord = serde_json::from_str(line).with_context(|| {
                format!(
                    "Invalid JSON on {} line {}",
                    file.display(),
                    idx + 1
                )
            })?;
            out.push(rec);
        }
    }
    Ok(out)
}

fn validate_manifest_records(records: &[ManifestRecord]) -> Result<()> {
    if records.is_empty() {
        anyhow::bail!("Manifest contains no records");
    }
    for (idx, rec) in records.iter().enumerate() {
        if rec.test_id.trim().is_empty() {
            anyhow::bail!("Record {} has empty test_id", idx + 1);
        }
        if let Some(language) = rec.language.as_deref() {
            if language.trim().is_empty() {
                anyhow::bail!("Record {} has empty language", idx + 1);
            }
            if normalize_language(language).is_none() {
                anyhow::bail!(
                    "Record {} has unsupported language '{}' (expected java or python)",
                    idx + 1,
                    language
                );
            }
        }
        if rec.coverage_report.as_deref().is_none() && rec.coverage_reports.is_empty() {
            anyhow::bail!(
                "Record {} for test '{}' must provide coverage_report or coverage_reports",
                idx + 1,
                rec.test_id
            );
        }
        let _ = rec.duration_ms;
    }
    Ok(())
}

fn build_test_language_index(records: &[ManifestRecord]) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for rec in records {
        let lang = if let Some(raw) = rec.language.as_deref() {
            normalize_language(raw)
                .ok_or_else(|| anyhow::anyhow!("Unsupported language '{}' for {}", raw, rec.test_id))?
        } else if rec.test_id.contains("::") {
            "python".to_string()
        } else {
            "java".to_string()
        };
        out.insert(rec.test_id.clone(), lang);
    }
    Ok(out)
}

fn normalize_language(raw: &str) -> Option<String> {
    let lowered = raw.trim().to_ascii_lowercase();
    match lowered.as_str() {
        "java" => Some("java".to_string()),
        "python" | "py" => Some("python".to_string()),
        _ => None,
    }
}

fn build_test_to_files_index(records: &[ManifestRecord]) -> Result<BTreeMap<String, BTreeSet<String>>> {
    let mut test_to_files: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for rec in records {
        let mut covered_files = BTreeSet::new();
        for coverage_path in rec.coverage_paths() {
            let mut coverage =
                covy_ingest::ingest_path(Path::new(coverage_path)).with_context(|| {
                format!(
                    "Failed to ingest coverage report '{}' for test '{}'",
                    coverage_path, rec.test_id
                )
            })?;
            covy_core::pathmap::auto_normalize_paths(&mut coverage, None);
            for file in coverage.files.keys() {
                covered_files.insert(file.clone());
            }
        }
        test_to_files.insert(rec.test_id.clone(), covered_files);
    }

    Ok(test_to_files)
}

fn build_file_to_tests_index(
    test_to_files: &BTreeMap<String, BTreeSet<String>>,
) -> BTreeMap<String, BTreeSet<String>> {
    let mut file_to_tests: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (test_id, files) in test_to_files {
        for file in files {
            file_to_tests
                .entry(file.clone())
                .or_default()
                .insert(test_id.clone());
        }
    }
    file_to_tests
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_manifest_records_success() {
        let records = vec![ManifestRecord {
            test_id: "com.foo.BarTest".to_string(),
            language: Some("java".to_string()),
            duration_ms: Some(123),
            coverage_report: Some("reports/bar.xml".to_string()),
            coverage_reports: Vec::new(),
        }];
        assert!(validate_manifest_records(&records).is_ok());
    }

    #[test]
    fn test_validate_manifest_records_missing_coverage() {
        let records = vec![ManifestRecord {
            test_id: "com.foo.BarTest".to_string(),
            language: None,
            duration_ms: None,
            coverage_report: None,
            coverage_reports: Vec::new(),
        }];
        let err = validate_manifest_records(&records).unwrap_err();
        assert!(err
            .to_string()
            .contains("must provide coverage_report or coverage_reports"));
    }

    #[test]
    fn test_manifest_record_coverage_paths() {
        let rec = ManifestRecord {
            test_id: "t".to_string(),
            language: None,
            duration_ms: None,
            coverage_report: Some("a.info".to_string()),
            coverage_reports: vec!["b.info".to_string()],
        };
        assert_eq!(rec.coverage_paths(), vec!["a.info", "b.info"]);
    }

    #[test]
    fn test_build_file_to_tests_index() {
        let mut test_to_files: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        test_to_files
            .entry("t1".to_string())
            .or_default()
            .insert("src/a.rs".to_string());
        test_to_files
            .entry("t2".to_string())
            .or_default()
            .insert("src/a.rs".to_string());
        test_to_files
            .entry("t2".to_string())
            .or_default()
            .insert("src/b.rs".to_string());

        let idx = build_file_to_tests_index(&test_to_files);
        assert_eq!(idx["src/a.rs"].len(), 2);
        assert_eq!(idx["src/b.rs"].len(), 1);
    }

    #[test]
    fn test_build_test_language_index_infers_python_nodeid() {
        let records = vec![ManifestRecord {
            test_id: "tests/test_a.py::test_x".to_string(),
            language: None,
            duration_ms: None,
            coverage_report: Some("a.info".to_string()),
            coverage_reports: Vec::new(),
        }];
        let map = build_test_language_index(&records).unwrap();
        assert_eq!(map["tests/test_a.py::test_x"], "python");
    }

    #[test]
    fn test_normalize_language() {
        assert_eq!(normalize_language("java"), Some("java".to_string()));
        assert_eq!(normalize_language("py"), Some("python".to_string()));
        assert_eq!(normalize_language("ruby"), None);
    }
}
