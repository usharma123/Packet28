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

/// Build a test impact map (testmap) from manifest records and write it to disk.
///
/// This command resolves manifest globs, parses and validates manifest records, constructs
/// test-to-file and file-to-test indices (including per-test language inference), populates
/// testmap metadata, serializes the index, and writes the resulting testmap to the
/// configured output path.
///
/// # Examples
///
/// ```no_run
/// use crate::{TestmapArgs, TestmapCommands, TestmapBuildArgs};
///
/// let args = TestmapArgs {
///     command: TestmapCommands::Build(TestmapBuildArgs {
///         manifest: vec!["manifests/*.ndjson".to_string()],
///         output: ".covy/state/testmap.bin".to_string(),
///         timings_output: ".covy/state/testtimings.bin".to_string(),
///     }),
/// };
///
/// // `run` returns `Ok(0)` on success or an error on failure.
/// let _ = run(args, "/path/to/config");
/// ```
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
    /// Collects all coverage file paths associated with this manifest record.
    ///
    /// This returns a vector of string slices containing the single `coverage_report` (if present)
    /// followed by each entry in `coverage_reports` in the same order they are stored.
    ///
    /// # Examples
    ///
    /// ```
    /// let rec = ManifestRecord {
    ///     test_id: "t".into(),
    ///     language: None,
    ///     duration_ms: None,
    ///     coverage_report: Some("cov1.xml".into()),
    ///     coverage_reports: vec!["cov2.xml".into(), "cov3.xml".into()],
    /// };
    /// let paths = rec.coverage_paths();
    /// assert_eq!(paths, vec!["cov1.xml", "cov2.xml", "cov3.xml"]);
    /// ```
    ///
    /// # Returns
    ///
    /// A `Vec<&str>` containing the coverage paths for this record; empty if none are present.
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

/// Expands glob patterns and returns all matching file paths.
///
/// Given a slice of glob pattern strings, returns a vector containing every path
/// that matches any of the patterns. Patterns that match no files produce a
/// warning; invalid glob patterns produce an error.
///
/// # Errors
///
/// Returns an error if any provided pattern is not a valid glob.
///
/// # Examples
///
/// ```
/// let patterns = vec!["src/**/*.rs".to_string()];
/// let files = resolve_globs(&patterns).unwrap();
/// assert!(files.iter().any(|p| p.extension().and_then(|e| e.to_str()) == Some("rs")));
/// ```
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

/// Read newline-delimited JSON manifest records from the provided files.
///
/// Each non-empty line in each file is parsed as a `ManifestRecord` and
/// returned in source order across the files. Returns an error if any file
/// cannot be read or any non-empty line contains invalid JSON; error messages
/// include the file path and the offending line number when applicable.
///
/// # Examples
///
/// ```
/// use std::path::PathBuf;
///
/// let path = std::env::temp_dir().join("test_manifest.jsonl");
/// std::fs::write(&path, r#"{"test_id":"t1","coverage_report":"c"}\n"#).unwrap();
/// let records = read_manifest_records(&[path.clone()]).unwrap();
/// assert_eq!(records[0].test_id, "t1");
/// let _ = std::fs::remove_file(path);
/// ```
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

/// Validates a list of manifest records and returns an error if any record is invalid.
///
/// Checks performed:
/// - The manifest must contain at least one record.
/// - Each record must have a non-empty `test_id`.
/// - If a `language` is provided it must be non-empty and one of the supported languages (`java` or `python`).
/// - Each record must provide either `coverage_report` or at least one `coverage_reports` entry.
///
/// # Examples
///
/// ```
/// let rec = ManifestRecord {
///     test_id: "com.example.Test#testSomething".to_string(),
///     language: Some("java".to_string()),
///     duration_ms: None,
///     coverage_report: Some("coverage.xml".to_string()),
///     coverage_reports: Vec::new(),
/// };
/// assert!(validate_manifest_records(&[rec]).is_ok());
/// ```
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

/// Builds a mapping from each test's ID to its normalized language identifier.
///
/// For records that include a `language`, the value is validated and normalized
/// (e.g., `"py"` -> `"python"`). If `language` is absent, the function
/// infers `"python"` for test IDs containing `"::"` and `"java"` otherwise.
///
/// # Returns
///
/// A `BTreeMap` where keys are test IDs and values are normalized language
/// names; returns an `Err` if any record specifies an unsupported language.
///
/// # Examples
///
/// ```
/// use std::collections::BTreeMap;
/// let records = vec![
///     ManifestRecord {
///         test_id: "com.example.Test#test".to_string(),
///         language: Some("java".to_string()),
///         duration_ms: None,
///         coverage_report: Some("cov".to_string()),
///         coverage_reports: vec![],
///     },
///     ManifestRecord {
///         test_id: "tests::test_function".to_string(),
///         language: None,
///         duration_ms: None,
///         coverage_report: Some("cov2".to_string()),
///         coverage_reports: vec![],
///     },
/// ];
/// let idx = build_test_language_index(&records).unwrap();
/// assert_eq!(idx.get("com.example.Test#test").map(String::as_str), Some("java"));
/// assert_eq!(idx.get("tests::test_function").map(String::as_str), Some("python"));
/// ```
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

/// Normalize a language name to a canonical identifier.
///
/// Recognizes common spellings and shorthands and returns the canonical lowercase
/// identifier when supported.
///
/// # Examples
///
/// ```
/// assert_eq!(normalize_language("Java"), Some("java".to_string()));
/// assert_eq!(normalize_language("py"), Some("python".to_string()));
/// assert_eq!(normalize_language(" RUBY "), None);
/// ```
///
/// # Returns
///
/// `Some("java")` or `Some("python")` when the input maps to a supported language, `None` otherwise.
fn normalize_language(raw: &str) -> Option<String> {
    let lowered = raw.trim().to_ascii_lowercase();
    match lowered.as_str() {
        "java" => Some("java".to_string()),
        "python" | "py" => Some("python".to_string()),
        _ => None,
    }
}

/// Builds an index mapping each test ID to the set of source file paths covered by that test.
///
/// For each manifest record this function ingests the record's coverage reports, normalizes
/// the reported file paths, and collects the unique set of covered files for that test.
///
/// # Returns
///
/// A map from `test_id` to a `BTreeSet` containing the normalized file path keys covered by that test.
///
/// # Errors
///
/// Returns an error if ingesting any coverage report fails; the error will include the
/// failing coverage report path and the associated `test_id`.
///
/// # Examples
///
/// ```
/// use std::collections::{BTreeMap, BTreeSet};
///
/// // Calling with no records returns an empty index.
/// let index: BTreeMap<String, BTreeSet<String>> = build_test_to_files_index(&[]).unwrap();
/// assert!(index.is_empty());
/// ```
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

/// Builds a mapping from file path to the set of test IDs that cover that file.
///
/// Given a map from test ID to the set of files it covers, returns a new map
/// where each file is mapped to the set of test IDs that reference it.
///
/// # Examples
///
/// ```
/// use std::collections::{BTreeMap, BTreeSet};
///
/// let mut test_to_files: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
/// test_to_files.insert(
///     "test1".to_string(),
///     vec!["file1".to_string(), "file2".to_string()].into_iter().collect(),
/// );
/// test_to_files.insert(
///     "test2".to_string(),
///     vec!["file2".to_string(), "file3".to_string()].into_iter().collect(),
/// );
///
/// let file_to_tests = build_file_to_tests_index(&test_to_files);
///
/// assert_eq!(file_to_tests.get("file1").unwrap().len(), 1);
/// assert_eq!(file_to_tests.get("file2").unwrap().len(), 2);
/// assert!(file_to_tests.get("file3").unwrap().contains("test2"));
/// ```
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