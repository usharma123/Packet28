use anyhow::Result;
use clap::Args;
use covy_core::CovyConfig;
use std::collections::BTreeSet;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Args)]
pub struct ImpactArgs {
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

pub fn run(args: ImpactArgs, config_path: &str) -> Result<i32> {
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

    let diffs = covy_core::diff::git_diff(base, head)?;
    let mut result = covy_core::impact::select_impacted_tests(&map, &diffs);
    let stale = is_stale(map.metadata.generated_at, config.impact.fresh_hours);
    apply_policy(&mut result, &diffs, &config, stale)?;

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

    if args.print_command {
        let command = if result.selected_tests.is_empty() {
            "echo \"no impacted tests\"".to_string()
        } else {
            format!("mvn -Dtest={} test", result.selected_tests.join(","))
        };
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

    if config.impact.fallback_mode.eq_ignore_ascii_case("fail-closed")
        && !result.missing_mappings.is_empty()
    {
        anyhow::bail!(
            "Impact mapping missing for {} changed file(s) in fail-closed mode",
            result.missing_mappings.len()
        );
    }

    Ok(())
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

        apply_policy(&mut result, &diffs, &cfg, true).unwrap();
        assert!(result.selected_tests.contains(&"t1".to_string()));
        assert!(result.selected_tests.contains(&"smoke::always".to_string()));
        assert!(result.selected_tests.contains(&"smoke::stale".to_string()));
        assert!(result.stale);
        assert!((result.confidence - 0.0).abs() < f64::EPSILON);
    }
}
