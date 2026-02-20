use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use covy_core::CovyConfig;

#[derive(Args)]
pub struct ShardArgs {
    #[command(subcommand)]
    pub command: ShardCommands,
}

#[derive(Subcommand)]
pub enum ShardCommands {
    /// Plan test shards for CI runners
    Plan(ShardPlanArgs),
}

#[derive(Args)]
pub struct ShardPlanArgs {
    /// Number of shards
    #[arg(long)]
    pub shards: usize,

    /// Input tests file
    #[arg(long)]
    pub tests_file: Option<String>,

    /// Impact JSON output file (selected_tests field)
    #[arg(long)]
    pub impact_json: Option<String>,

    /// Timing history path
    #[arg(long)]
    pub timings: Option<String>,

    /// Fallback duration (seconds) for unknown tests
    #[arg(long)]
    pub unknown_test_seconds: Option<f64>,

    /// Emit JSON output
    #[arg(long)]
    pub json: bool,

    /// Directory for shard output files
    #[arg(long)]
    pub write_files: Option<String>,
}

/// Plan and emit test shards according to the provided CLI arguments and configuration.
///
/// Loads configuration from `config_path` (falling back to defaults), reads tests and timing
/// history as specified by `args`, builds a timed-job list, computes a shard plan using a
/// least-processing-time strategy, and then either writes per-shard files and/or prints the
/// plan as pretty JSON or human-readable text.
///
/// # Returns
///
/// `0` on success.
///
/// # Examples
///
/// ```no_run
/// use crate::cmd_shard::{ShardArgs, ShardCommands, ShardPlanArgs};
///
/// let args = ShardArgs {
///     command: ShardCommands::Plan(ShardPlanArgs {
///         shards: 2,
///         tests_file: Some("tests.txt".into()),
///         impact_json: None,
///         timings: None,
///         unknown_test_seconds: None,
///         json: false,
///         write_files: None,
///     }),
/// };
///
/// // Call with a path to a config file (may be absent; defaults are used).
/// let _ = crate::cmd_shard::run(args, "covy.toml");
/// ```
pub fn run(args: ShardArgs, config_path: &str) -> Result<i32> {
    let config = CovyConfig::load(Path::new(config_path)).unwrap_or_default();
    match args.command {
        ShardCommands::Plan(plan) => {
            let tests = load_tests(&plan)?;
            if tests.is_empty() {
                anyhow::bail!("No tests provided for shard planning");
            }
            let timings_path = plan
                .timings
                .as_deref()
                .unwrap_or(&config.shard.timings_path)
                .to_string();
            let timings = load_timings(Path::new(&timings_path))?;
            let unknown_seconds = plan
                .unknown_test_seconds
                .unwrap_or(config.shard.unknown_test_seconds);
            let unknown_ms = (unknown_seconds * 1000.0) as u64;
            let jobs = covy_core::shard::build_timed_jobs(&tests, &timings, unknown_ms);
            let shard_plan = covy_core::shard::plan_shards_lpt(&jobs, plan.shards);

            if let Some(dir) = plan.write_files.as_deref() {
                write_shard_files(dir, &shard_plan)?;
            }

            if plan.json {
                println!("{}", serde_json::to_string_pretty(&shard_plan)?);
            } else {
                render_text(&shard_plan);
            }

            Ok(0)
        }
    }
}

/// Selects and loads test identifiers according to provided shard plan arguments.
///
/// If `args.tests_file` is set, reads tests from that newline-separated file.
/// If `args.impact_json` is set, parses the impact JSON and extracts `selected_tests`.
///
/// # Errors
///
/// Returns an error if both or neither of `--tests-file` and `--impact-json` are provided,
/// or if reading/parsing the selected input fails.
///
/// # Examples
///
/// ```
/// let args = ShardPlanArgs {
///     shards: 1,
///     tests_file: Some("tests.txt".into()),
///     impact_json: None,
///     timings: None,
///     unknown_test_seconds: None,
///     json: false,
///     write_files: None,
/// };
/// let tests = load_tests(&args).unwrap();
/// ```
fn load_tests(args: &ShardPlanArgs) -> Result<Vec<String>> {
    match (&args.tests_file, &args.impact_json) {
        (Some(path), None) => load_tests_from_file(Path::new(path)),
        (None, Some(path)) => load_tests_from_impact_json(Path::new(path)),
        (Some(_), Some(_)) => anyhow::bail!("Provide either --tests-file or --impact-json, not both"),
        (None, None) => anyhow::bail!("One of --tests-file or --impact-json is required"),
    }
}

/// Reads a tests file and returns each non-empty line as a trimmed `String`.
///
/// The function returns an error if the file cannot be read; successful result contains
/// all lines from the file with surrounding whitespace removed and empty lines excluded.
///
/// # Examples
///
/// ```
/// use std::fs;
/// use std::path::Path;
///
/// let path = Path::new("tests_example.txt");
/// fs::write(&path, "a\n\nb\n").unwrap();
/// let tests = load_tests_from_file(&path).unwrap();
/// assert_eq!(tests, vec!["a".to_string(), "b".to_string()]);
/// fs::remove_file(&path).unwrap();
/// ```
fn load_tests_from_file(path: &Path) -> Result<Vec<String>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read tests file {}", path.display()))?;
    let tests = content
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect();
    Ok(tests)
}

/// Load selected test identifiers from an impact JSON file.
///
/// Reads the file at `path`, parses it as an `ImpactResult`, and returns the
/// `selected_tests` array extracted from the JSON. Returns an error if the
/// file cannot be read or if the content cannot be parsed as impact JSON.
///
/// # Examples
///
/// ```
/// use std::path::Path;
/// let path = std::env::temp_dir().join("covy_impact_example.json");
/// std::fs::write(&path, r#"{"selected_tests": ["a::test1", "b::test2"]}"#).unwrap();
/// let tests = crate::cmd_shard::load_tests_from_impact_json(Path::new(&path)).unwrap();
/// assert_eq!(tests, vec!["a::test1".to_string(), "b::test2".to_string()]);
/// ```
fn load_tests_from_impact_json(path: &Path) -> Result<Vec<String>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read impact JSON {}", path.display()))?;
    let impact: covy_core::impact::ImpactResult =
        serde_json::from_str(&content).context("Failed to parse impact JSON")?;
    Ok(impact.selected_tests)
}

/// Load test timing history from `path`, returning a default empty history when the
/// file does not exist.
///
/// If the file exists, it is read and deserialized into `covy_core::testmap::TestTimingHistory`.
/// I/O and deserialization errors are returned as `anyhow::Error`.
///
/// # Examples
///
/// ```no_run
/// use std::path::Path;
/// // Nonexistent path yields the default timing history
/// let p = Path::new("nonexistent_timings_file.bin");
/// let hist = crate::cmd_shard::load_timings(p).unwrap();
/// assert_eq!(hist, covy_core::testmap::TestTimingHistory::default());
/// ```
fn load_timings(path: &Path) -> Result<covy_core::testmap::TestTimingHistory> {
    if !path.exists() {
        return Ok(covy_core::testmap::TestTimingHistory::default());
    }
    let bytes = std::fs::read(path)
        .with_context(|| format!("Failed to read timings file {}", path.display()))?;
    covy_core::cache::deserialize_test_timings(&bytes).map_err(Into::into)
}

/// Write each shard's tests into separate numbered files under `dir`.
///
/// Creates `dir` if it does not exist. For each shard in `plan.shards` this
/// function writes a file named `shard-<n>.txt` where `<n>` is the shard index
/// starting at 1. Each file contains the shard's tests one-per-line and ends
/// with a trailing newline. If a shard has no tests an empty file is created.
///
/// # Examples
///
/// ```
/// use std::fs;
/// use tempfile::tempdir;
/// use covy_core::shard::{Shard, ShardPlan};
///
/// let td = tempdir().unwrap();
/// let out_dir = td.path().to_str().unwrap();
///
/// let plan = ShardPlan {
///     shards: vec![
///         Shard { id: 0, tests: vec!["a::test1".into(), "a::test2".into()], total_ms: 10 },
///         Shard { id: 1, tests: vec![], total_ms: 0 },
///     ],
/// };
///
/// // write files
/// crate::write_shard_files(out_dir, &plan).unwrap();
///
/// // verify contents
/// let s1 = fs::read_to_string(td.path().join("shard-1.txt")).unwrap();
/// assert_eq!(s1, "a::test1\na::test2\n");
/// let s2 = fs::read_to_string(td.path().join("shard-2.txt")).unwrap();
/// assert_eq!(s2, "");
/// ```
fn write_shard_files(dir: &str, plan: &covy_core::shard::ShardPlan) -> Result<()> {
    let dir = PathBuf::from(dir);
    std::fs::create_dir_all(&dir)?;
    for shard in &plan.shards {
        let path = dir.join(format!("shard-{}.txt", shard.id + 1));
        let content = if shard.tests.is_empty() {
            String::new()
        } else {
            format!("{}\n", shard.tests.join("\n"))
        };
        std::fs::write(path, content)?;
    }
    Ok(())
}

/// Render a shard plan as human-readable text to standard output.
///
/// Prints a one-line summary with shard count, total predicted duration (ms), and makespan (ms),
/// then prints each shard's header (1-based id, test count, predicted duration) followed by each
/// test identifier on its own line.
///
/// # Examples
///
/// ```
/// use covy_core::shard::{Shard, ShardPlan};
///
/// let shard = Shard {
///     id: 0,
///     tests: vec!["a::test_one".into(), "b::test_two".into()],
///     predicted_duration_ms: 150,
/// };
/// let plan = ShardPlan {
///     shards: vec![shard],
///     total_predicted_duration_ms: 150,
///     makespan_ms: 150,
/// };
///
/// // Prints a human-readable summary to stdout.
/// crate::cmd_shard::render_text(&plan);
/// ```
fn render_text(plan: &covy_core::shard::ShardPlan) {
    println!(
        "shards={} total_ms={} makespan_ms={}",
        plan.shards.len(),
        plan.total_predicted_duration_ms,
        plan.makespan_ms
    );
    for shard in &plan.shards {
        println!(
            "shard={} tests={} predicted_ms={}",
            shard.id + 1,
            shard.tests.len(),
            shard.predicted_duration_ms
        );
        for test in &shard.tests {
            println!("{test}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_tests_from_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("tests.txt");
        std::fs::write(&path, "a\n\nb\n").unwrap();
        let tests = load_tests_from_file(&path).unwrap();
        assert_eq!(tests, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn test_load_tests_from_impact_json_with_python_nodeids() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("impact.json");
        let payload = serde_json::json!({
            "selected_tests": ["tests/test_x.py::test_a", "tests/test_y.py::test_b"],
            "smoke_tests": [],
            "missing_mappings": [],
            "stale": false,
            "confidence": 1.0,
            "escalate_full_suite": false
        });
        std::fs::write(&path, serde_json::to_string(&payload).unwrap()).unwrap();
        let tests = load_tests_from_impact_json(&path).unwrap();
        assert_eq!(
            tests,
            vec![
                "tests/test_x.py::test_a".to_string(),
                "tests/test_y.py::test_b".to_string()
            ]
        );
    }
}