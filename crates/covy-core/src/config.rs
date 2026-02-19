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
