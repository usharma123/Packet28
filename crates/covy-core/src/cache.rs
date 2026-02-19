use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::diagnostics::{DiagnosticsData, DiagnosticsFormat, Issue, Severity};
use crate::error::CovyError;
use crate::model::CoverageData;

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

/// Serialize CoverageData to bytes for storage.
pub fn serialize_coverage(data: &CoverageData) -> Result<Vec<u8>, CovyError> {
    // We store a simplified version since RoaringBitmap isn't directly bincode-serializable
    let mut out = Vec::new();

    // Write file count
    let file_count = data.files.len() as u32;
    out.extend_from_slice(&file_count.to_le_bytes());

    for (path, fc) in &data.files {
        // Write path
        let path_bytes = path.as_bytes();
        out.extend_from_slice(&(path_bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(path_bytes);

        // Write covered bitmap
        let mut covered_buf = Vec::new();
        fc.lines_covered
            .serialize_into(&mut covered_buf)
            .map_err(|e| CovyError::Cache(format!("bitmap serialize error: {e}")))?;
        out.extend_from_slice(&(covered_buf.len() as u32).to_le_bytes());
        out.extend_from_slice(&covered_buf);

        // Write instrumented bitmap
        let mut instr_buf = Vec::new();
        fc.lines_instrumented
            .serialize_into(&mut instr_buf)
            .map_err(|e| CovyError::Cache(format!("bitmap serialize error: {e}")))?;
        out.extend_from_slice(&(instr_buf.len() as u32).to_le_bytes());
        out.extend_from_slice(&instr_buf);
    }

    out.extend_from_slice(&data.timestamp.to_le_bytes());
    Ok(out)
}

/// Deserialize CoverageData from bytes.
pub fn deserialize_coverage(data: &[u8]) -> Result<CoverageData, CovyError> {
    use roaring::RoaringBitmap;
    use std::io::Cursor;

    let mut pos = 0;
    let read_u32 = |pos: &mut usize| -> Result<u32, CovyError> {
        if *pos + 4 > data.len() {
            return Err(CovyError::Cache("unexpected EOF".to_string()));
        }
        let val = u32::from_le_bytes(data[*pos..*pos + 4].try_into().unwrap());
        *pos += 4;
        Ok(val)
    };

    let file_count = read_u32(&mut pos)?;
    let mut files = std::collections::BTreeMap::new();

    for _ in 0..file_count {
        let path_len = read_u32(&mut pos)? as usize;
        if pos + path_len > data.len() {
            return Err(CovyError::Cache("unexpected EOF".to_string()));
        }
        let path = String::from_utf8_lossy(&data[pos..pos + path_len]).to_string();
        pos += path_len;

        let covered_len = read_u32(&mut pos)? as usize;
        if pos + covered_len > data.len() {
            return Err(CovyError::Cache("unexpected EOF".to_string()));
        }
        let lines_covered =
            RoaringBitmap::deserialize_from(Cursor::new(&data[pos..pos + covered_len]))
                .map_err(|e| CovyError::Cache(format!("bitmap deserialize error: {e}")))?;
        pos += covered_len;

        let instr_len = read_u32(&mut pos)? as usize;
        if pos + instr_len > data.len() {
            return Err(CovyError::Cache("unexpected EOF".to_string()));
        }
        let lines_instrumented =
            RoaringBitmap::deserialize_from(Cursor::new(&data[pos..pos + instr_len]))
                .map_err(|e| CovyError::Cache(format!("bitmap deserialize error: {e}")))?;
        pos += instr_len;

        files.insert(
            path,
            crate::model::FileCoverage {
                lines_covered,
                lines_instrumented,
                branches: std::collections::BTreeMap::new(),
                functions: std::collections::BTreeMap::new(),
            },
        );
    }

    let timestamp = if pos + 8 <= data.len() {
        u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap())
    } else {
        0
    };

    Ok(CoverageData {
        files,
        format: None,
        timestamp,
    })
}

/// Serialize DiagnosticsData for storage.
pub fn serialize_diagnostics(data: &DiagnosticsData) -> Result<Vec<u8>, CovyError> {
    let stored = StoredDiagnosticsData::from_runtime(data);
    bincode::serialize(&stored)
        .map_err(|e| CovyError::Cache(format!("Failed to serialize diagnostics: {e}")))
}

/// Deserialize DiagnosticsData from bytes.
pub fn deserialize_diagnostics(data: &[u8]) -> Result<DiagnosticsData, CovyError> {
    let stored: StoredDiagnosticsData = bincode::deserialize(data)
        .map_err(|e| CovyError::Cache(format!("Failed to deserialize diagnostics: {e}")))?;
    Ok(stored.into_runtime())
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct StoredIssue {
    path: String,
    line: u32,
    column: Option<u32>,
    end_line: Option<u32>,
    severity: Severity,
    rule_id: String,
    message: String,
    source: String,
    fingerprint: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct StoredDiagnosticsData {
    issues_by_file: std::collections::BTreeMap<String, Vec<StoredIssue>>,
    format: Option<DiagnosticsFormat>,
    timestamp: u64,
}

impl StoredDiagnosticsData {
    fn from_runtime(data: &DiagnosticsData) -> Self {
        let mut issues_by_file = std::collections::BTreeMap::new();
        for (path, issues) in &data.issues_by_file {
            let stored: Vec<StoredIssue> = issues
                .iter()
                .map(|issue| StoredIssue {
                    path: issue.path.clone(),
                    line: issue.line,
                    column: issue.column,
                    end_line: issue.end_line,
                    severity: issue.severity,
                    rule_id: issue.rule_id.clone(),
                    message: issue.message.clone(),
                    source: issue.source.clone(),
                    fingerprint: issue.fingerprint.clone(),
                })
                .collect();
            issues_by_file.insert(path.clone(), stored);
        }

        Self {
            issues_by_file,
            format: data.format,
            timestamp: data.timestamp,
        }
    }

    fn into_runtime(self) -> DiagnosticsData {
        let mut issues_by_file = std::collections::BTreeMap::new();
        for (path, issues) in self.issues_by_file {
            let runtime: Vec<Issue> = issues
                .into_iter()
                .map(|issue| Issue {
                    path: issue.path,
                    line: issue.line,
                    column: issue.column,
                    end_line: issue.end_line,
                    severity: issue.severity,
                    rule_id: issue.rule_id,
                    message: issue.message,
                    source: issue.source,
                    fingerprint: issue.fingerprint,
                })
                .collect();
            issues_by_file.insert(path, runtime);
        }

        DiagnosticsData {
            issues_by_file,
            format: self.format,
            timestamp: self.timestamp,
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

    #[test]
    fn test_coverage_serialization_roundtrip() {
        let mut data = CoverageData::new();
        let mut fc = crate::model::FileCoverage::new();
        fc.lines_covered.insert(1);
        fc.lines_covered.insert(5);
        fc.lines_instrumented.insert(1);
        fc.lines_instrumented.insert(2);
        fc.lines_instrumented.insert(5);
        data.files.insert("test.rs".to_string(), fc);

        let bytes = serialize_coverage(&data).unwrap();
        let restored = deserialize_coverage(&bytes).unwrap();
        assert_eq!(restored.files.len(), 1);
        let rfc = &restored.files["test.rs"];
        assert_eq!(rfc.lines_covered.len(), 2);
        assert_eq!(rfc.lines_instrumented.len(), 3);
    }

    #[test]
    fn test_diagnostics_serialization_roundtrip() {
        let mut data = DiagnosticsData::new();
        data.issues_by_file.insert(
            "src/main.rs".to_string(),
            vec![crate::diagnostics::Issue {
                path: "src/main.rs".to_string(),
                line: 10,
                column: Some(2),
                end_line: Some(10),
                severity: crate::diagnostics::Severity::Error,
                rule_id: "R001".to_string(),
                message: "boom".to_string(),
                source: "tool".to_string(),
                fingerprint: "fp-1".to_string(),
            }],
        );

        let bytes = serialize_diagnostics(&data).unwrap();
        let restored = deserialize_diagnostics(&bytes).unwrap();
        assert_eq!(restored.total_issues(), 1);
        assert_eq!(restored.issues_by_file["src/main.rs"][0].rule_id, "R001");
    }
}
