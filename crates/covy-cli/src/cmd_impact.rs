use anyhow::Result;
use clap::{Args, Subcommand};
use covy_core::CovyConfig;
use std::collections::BTreeSet;
use std::path::Path;
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
pub struct ImpactRecordArgs {}

#[derive(Args, Default)]
pub struct ImpactPlanArgs {}

#[derive(Args, Default)]
pub struct ImpactRunArgs {}

pub fn run(args: ImpactArgs, config_path: &str) -> Result<i32> {
    match args.command {
        Some(ImpactCommand::Record(_)) => {
            anyhow::bail!("`covy impact record` is not implemented yet")
        }
        Some(ImpactCommand::Plan(_)) => {
            anyhow::bail!("`covy impact plan` is not implemented yet")
        }
        Some(ImpactCommand::Run(_)) => {
            anyhow::bail!("`covy impact run` is not implemented yet")
        }
        None => run_legacy(args.legacy, config_path),
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
}
