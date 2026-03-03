use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::error::CovyError;

/// File-system cache for coverage data keyed by hash.
pub struct CoverageCache {
    dir: PathBuf,
    max_age: Duration,
}

impl CoverageCache {
    pub fn new(dir: &Path, max_age_days: u32) -> Self {
        Self {
            dir: dir.to_path_buf(),
            max_age: Duration::from_secs(max_age_days as u64 * 86400),
        }
    }

    /// Compute cache key from base hash, head hash, and coverage hash.
    pub fn cache_key(base_hash: &str, head_hash: &str, coverage_hash: &str) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(base_hash.as_bytes());
        hasher.update(head_hash.as_bytes());
        hasher.update(coverage_hash.as_bytes());
        hasher.finalize().to_hex().to_string()
    }

    /// Try to load cached coverage data.
    pub fn get(&self, key: &str) -> Result<Option<CachedResult>, CovyError> {
        let path = self.dir.join(key);
        if !path.exists() {
            return Ok(None);
        }

        // Check age
        let metadata = std::fs::metadata(&path)?;
        if let Ok(modified) = metadata.modified() {
            if let Ok(age) = SystemTime::now().duration_since(modified) {
                if age > self.max_age {
                    let _ = std::fs::remove_file(&path);
                    return Ok(None);
                }
            }
        }

        let data = std::fs::read(&path)?;
        let result: CachedResult = bincode::deserialize(&data)
            .map_err(|e| CovyError::Cache(format!("Failed to deserialize cache: {e}")))?;
        Ok(Some(result))
    }

    /// Store a result in the cache.
    pub fn put(&self, key: &str, result: &CachedResult) -> Result<(), CovyError> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(key);
        let data = bincode::serialize(result)
            .map_err(|e| CovyError::Cache(format!("Failed to serialize cache: {e}")))?;
        std::fs::write(path, data)?;
        Ok(())
    }

    /// Evict entries older than max_age.
    pub fn evict(&self) -> Result<u32, CovyError> {
        if !self.dir.exists() {
            return Ok(0);
        }
        let mut count = 0;
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            if let Ok(modified) = entry.metadata()?.modified() {
                if let Ok(age) = SystemTime::now().duration_since(modified) {
                    if age > self.max_age {
                        let _ = std::fs::remove_file(entry.path());
                        count += 1;
                    }
                }
            }
        }
        Ok(count)
    }
}

/// Cached gate evaluation result.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CachedResult {
    pub passed: bool,
    pub total_coverage_pct: Option<f64>,
    pub changed_coverage_pct: Option<f64>,
    pub new_file_coverage_pct: Option<f64>,
    pub violations: Vec<String>,
    #[serde(default)]
    pub issue_counts: Option<crate::model::IssueGateCounts>,
}

impl From<&crate::model::QualityGateResult> for CachedResult {
    fn from(r: &crate::model::QualityGateResult) -> Self {
        Self {
            passed: r.passed,
            total_coverage_pct: r.total_coverage_pct,
            changed_coverage_pct: r.changed_coverage_pct,
            new_file_coverage_pct: r.new_file_coverage_pct,
            violations: r.violations.clone(),
            issue_counts: r.issue_counts.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_cache_roundtrip() {
        let dir = TempDir::new().unwrap();
        let cache = CoverageCache::new(dir.path(), 30);

        let result = CachedResult {
            passed: true,
            total_coverage_pct: Some(85.0),
            changed_coverage_pct: Some(90.0),
            new_file_coverage_pct: None,
            violations: vec![],
            issue_counts: None,
        };

        let key = CoverageCache::cache_key("abc", "def", "ghi");
        cache.put(&key, &result).unwrap();
        let loaded = cache.get(&key).unwrap().unwrap();
        assert!(loaded.passed);
        assert_eq!(loaded.total_coverage_pct, Some(85.0));
    }
}
