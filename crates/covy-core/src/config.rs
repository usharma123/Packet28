use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

use crate::error::CovyError;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CovyConfig {
    pub project: ProjectConfig,
    pub ingest: IngestConfig,
    pub diff: DiffConfig,
    pub gate: GateConfig,
    pub report: ReportConfig,
    pub cache: CacheConfig,
    pub impact: ImpactConfig,
    pub shard: ShardConfig,
    pub merge: MergeConfig,
    pub path_mapping: PathMappingConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ProjectConfig {
    pub name: String,
    pub source_root: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct IngestConfig {
    pub report_paths: Vec<String>,
    pub strip_prefixes: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DiffConfig {
    pub base: String,
    pub head: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct GateConfig {
    pub fail_under_total: Option<f64>,
    pub fail_under_changed: Option<f64>,
    pub fail_under_new: Option<f64>,
    #[serde(default)]
    pub issues: IssueGateConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct IssueGateConfig {
    pub max_new_errors: Option<u32>,
    pub max_new_warnings: Option<u32>,
    pub max_new_issues: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ReportConfig {
    pub format: String,
    pub show_missing: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CacheConfig {
    pub enabled: bool,
    pub dir: String,
    pub max_age_days: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ImpactConfig {
    pub testmap_path: String,
    pub fresh_hours: u32,
    pub full_suite_threshold: f64,
    pub fallback_mode: String,
    pub smoke: ImpactSmokeConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ImpactSmokeConfig {
    pub always: Vec<String>,
    pub stale_extra: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ShardConfig {
    pub timings_path: String,
    pub algorithm: String,
    pub unknown_test_seconds: f64,
    pub tiers: ShardTiersConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ShardTiersConfig {
    pub pr: ShardTierConfig,
    pub nightly: ShardTierConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ShardTierConfig {
    pub exclude_tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MergeConfig {
    pub strict: bool,
    pub output_coverage: String,
    pub output_issues: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PathMappingConfig {
    pub rules: BTreeMap<String, String>,
}

// Defaults

impl Default for CovyConfig {
    fn default() -> Self {
        Self {
            project: ProjectConfig::default(),
            ingest: IngestConfig::default(),
            diff: DiffConfig::default(),
            gate: GateConfig::default(),
            report: ReportConfig::default(),
            cache: CacheConfig::default(),
            impact: ImpactConfig::default(),
            shard: ShardConfig::default(),
            merge: MergeConfig::default(),
            path_mapping: PathMappingConfig::default(),
        }
    }
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            source_root: ".".to_string(),
        }
    }
}

impl Default for IngestConfig {
    fn default() -> Self {
        Self {
            report_paths: Vec::new(),
            strip_prefixes: Vec::new(),
        }
    }
}

impl Default for DiffConfig {
    fn default() -> Self {
        Self {
            base: "main".to_string(),
            head: "HEAD".to_string(),
        }
    }
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            fail_under_total: None,
            fail_under_changed: None,
            fail_under_new: None,
            issues: IssueGateConfig::default(),
        }
    }
}

impl Default for IssueGateConfig {
    fn default() -> Self {
        Self {
            max_new_errors: None,
            max_new_warnings: None,
            max_new_issues: None,
        }
    }
}

impl Default for ReportConfig {
    fn default() -> Self {
        Self {
            format: "terminal".to_string(),
            show_missing: false,
        }
    }
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            dir: ".covy/cache".to_string(),
            max_age_days: 30,
        }
    }
}

impl Default for ImpactConfig {
    fn default() -> Self {
        Self {
            testmap_path: ".covy/state/testmap.bin".to_string(),
            fresh_hours: 24,
            full_suite_threshold: 0.40,
            fallback_mode: "fail-open".to_string(),
            smoke: ImpactSmokeConfig::default(),
        }
    }
}

impl Default for ImpactSmokeConfig {
    fn default() -> Self {
        Self {
            always: Vec::new(),
            stale_extra: Vec::new(),
        }
    }
}

impl Default for ShardConfig {
    fn default() -> Self {
        Self {
            timings_path: ".covy/state/testtimings.bin".to_string(),
            algorithm: "lpt".to_string(),
            unknown_test_seconds: 8.0,
            tiers: ShardTiersConfig::default(),
        }
    }
}

impl Default for ShardTiersConfig {
    fn default() -> Self {
        Self {
            pr: ShardTierConfig {
                exclude_tags: vec!["slow".to_string()],
            },
            nightly: ShardTierConfig::default(),
        }
    }
}

impl Default for ShardTierConfig {
    fn default() -> Self {
        Self {
            exclude_tags: Vec::new(),
        }
    }
}

impl Default for MergeConfig {
    fn default() -> Self {
        Self {
            strict: true,
            output_coverage: ".covy/state/latest.bin".to_string(),
            output_issues: ".covy/state/issues.bin".to_string(),
        }
    }
}

impl Default for PathMappingConfig {
    fn default() -> Self {
        Self {
            rules: BTreeMap::new(),
        }
    }
}

impl CovyConfig {
    /// Load config from a TOML file. Returns default config if file doesn't exist.
    pub fn load(path: &Path) -> Result<Self, CovyError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(path)?;
        let config: CovyConfig = toml::from_str(&content)?;
        Ok(config)
    }

    /// Search for covy.toml in the current directory and parents.
    pub fn find_and_load() -> Result<Self, CovyError> {
        let mut dir = std::env::current_dir()?;
        loop {
            let candidate = dir.join("covy.toml");
            if candidate.exists() {
                return Self::load(&candidate);
            }
            if !dir.pop() {
                break;
            }
        }
        Ok(Self::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_gate_issues_defaults() {
        let raw = r#"
            [gate]
            fail_under_total = 80.0
        "#;
        let config: CovyConfig = toml::from_str(raw).unwrap();
        assert_eq!(config.gate.fail_under_total, Some(80.0));
        assert_eq!(config.gate.issues.max_new_errors, None);
        assert_eq!(config.gate.issues.max_new_warnings, None);
        assert_eq!(config.gate.issues.max_new_issues, None);
    }

    #[test]
    fn test_deserialize_gate_issues_configured() {
        let raw = r#"
            [gate]
            fail_under_changed = 90.0

            [gate.issues]
            max_new_errors = 0
            max_new_warnings = 5
            max_new_issues = 8
        "#;
        let config: CovyConfig = toml::from_str(raw).unwrap();
        assert_eq!(config.gate.fail_under_changed, Some(90.0));
        assert_eq!(config.gate.issues.max_new_errors, Some(0));
        assert_eq!(config.gate.issues.max_new_warnings, Some(5));
        assert_eq!(config.gate.issues.max_new_issues, Some(8));
    }

    #[test]
    fn test_deserialize_impact_shard_merge_defaults() {
        let raw = r#"
            [project]
            name = "demo"
        "#;
        let config: CovyConfig = toml::from_str(raw).unwrap();
        assert_eq!(config.impact.testmap_path, ".covy/state/testmap.bin");
        assert_eq!(config.impact.fresh_hours, 24);
        assert!((config.impact.full_suite_threshold - 0.40).abs() < f64::EPSILON);
        assert_eq!(config.impact.fallback_mode, "fail-open");
        assert_eq!(config.shard.algorithm, "lpt");
        assert!((config.shard.unknown_test_seconds - 8.0).abs() < f64::EPSILON);
        assert_eq!(config.shard.tiers.pr.exclude_tags, vec!["slow".to_string()]);
        assert!(config.shard.tiers.nightly.exclude_tags.is_empty());
        assert!(config.merge.strict);
        assert_eq!(config.merge.output_coverage, ".covy/state/latest.bin");
    }
}
