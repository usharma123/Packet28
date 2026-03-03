use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::diagnostics::{DiagnosticsData, DiagnosticsFormat, Issue, Severity};
use crate::error::CovyError;
use crate::model::CoverageData;
use crate::testmap::{TestMapIndex, TestTimingHistory};

pub const DIAGNOSTICS_STATE_SCHEMA_VERSION: u16 = 2;
pub const DIAGNOSTICS_PATH_NORM_VERSION: u16 = 1;
pub const TESTMAP_SCHEMA_VERSION: u16 = 2;
pub const TESTTIMINGS_SCHEMA_VERSION: u16 = 1;
const DIAGNOSTICS_MAGIC: &[u8; 9] = b"COVYDIAG2";

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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct StoredTestTimingHistory {
    schema_version: u16,
    timings: TestTimingHistory,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct LegacyTestMapMetadataV1 {
    schema_version: u16,
    path_norm_version: u16,
    repo_root_id: Option<String>,
    generated_at: u64,
    granularity: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct LegacyTestMapIndexV1 {
    metadata: LegacyTestMapMetadataV1,
    test_language: std::collections::BTreeMap<String, String>,
    test_to_files: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    file_to_tests: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
}

/// Serialize TestMapIndex to bytes for storage.
pub fn serialize_testmap(index: &TestMapIndex) -> Result<Vec<u8>, CovyError> {
    let mut stored = index.clone();
    stored.metadata.schema_version = TESTMAP_SCHEMA_VERSION;
    bincode::serialize(&stored)
        .map_err(|e| CovyError::Cache(format!("Failed to serialize testmap: {e}")))
}

/// Deserialize TestMapIndex from bytes.
pub fn deserialize_testmap(data: &[u8]) -> Result<TestMapIndex, CovyError> {
    if let Ok(stored) = bincode::deserialize::<TestMapIndex>(data) {
        if stored.metadata.schema_version == TESTMAP_SCHEMA_VERSION {
            return Ok(stored);
        }
        if stored.metadata.schema_version == 1 {
            return Ok(normalize_v1_testmap(stored));
        }
        return Err(CovyError::Cache(format!(
            "Unsupported testmap schema version {} (expected {} or 1)",
            stored.metadata.schema_version, TESTMAP_SCHEMA_VERSION
        )));
    }

    let legacy: LegacyTestMapIndexV1 = bincode::deserialize(data)
        .map_err(|e| CovyError::Cache(format!("Failed to deserialize testmap: {e}")))?;
    Ok(normalize_v1_testmap(TestMapIndex {
        metadata: crate::testmap::TestMapMetadata {
            schema_version: legacy.metadata.schema_version,
            path_norm_version: legacy.metadata.path_norm_version,
            repo_root_id: legacy.metadata.repo_root_id,
            generated_at: legacy.metadata.generated_at,
            granularity: legacy.metadata.granularity,
            commit_sha: None,
            created_at: None,
            toolchain_fingerprint: None,
        },
        test_language: legacy.test_language,
        test_to_files: legacy.test_to_files,
        file_to_tests: legacy.file_to_tests,
        tests: Vec::new(),
        file_index: Vec::new(),
        coverage: Vec::new(),
    }))
}

fn normalize_v1_testmap(index: TestMapIndex) -> TestMapIndex {
    TestMapIndex {
        metadata: crate::testmap::TestMapMetadata {
            schema_version: index.metadata.schema_version,
            path_norm_version: index.metadata.path_norm_version,
            repo_root_id: index.metadata.repo_root_id,
            generated_at: index.metadata.generated_at,
            granularity: index.metadata.granularity,
            commit_sha: None,
            created_at: None,
            toolchain_fingerprint: None,
        },
        test_language: index.test_language,
        test_to_files: index.test_to_files,
        file_to_tests: index.file_to_tests,
        tests: Vec::new(),
        file_index: Vec::new(),
        coverage: Vec::new(),
    }
}

/// Serialize TestTimingHistory to bytes for storage.
pub fn serialize_test_timings(timings: &TestTimingHistory) -> Result<Vec<u8>, CovyError> {
    let stored = StoredTestTimingHistory {
        schema_version: TESTTIMINGS_SCHEMA_VERSION,
        timings: timings.clone(),
    };
    bincode::serialize(&stored)
        .map_err(|e| CovyError::Cache(format!("Failed to serialize test timings: {e}")))
}

/// Deserialize TestTimingHistory from bytes.
pub fn deserialize_test_timings(data: &[u8]) -> Result<TestTimingHistory, CovyError> {
    let stored: StoredTestTimingHistory = bincode::deserialize(data)
        .map_err(|e| CovyError::Cache(format!("Failed to deserialize test timings: {e}")))?;
    if stored.schema_version != TESTTIMINGS_SCHEMA_VERSION {
        return Err(CovyError::Cache(format!(
            "Unsupported test timings schema version {} (expected {})",
            stored.schema_version, TESTTIMINGS_SCHEMA_VERSION
        )));
    }
    Ok(stored.timings)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct DiagnosticsStateMetadata {
    pub schema_version: u16,
    pub path_norm_version: u16,
    pub normalized_paths: bool,
    pub repo_root_id: Option<String>,
}

impl DiagnosticsStateMetadata {
    pub fn normalized_for_repo_root(repo_root_id: Option<String>) -> Self {
        Self {
            schema_version: DIAGNOSTICS_STATE_SCHEMA_VERSION,
            path_norm_version: DIAGNOSTICS_PATH_NORM_VERSION,
            normalized_paths: true,
            repo_root_id,
        }
    }

    pub fn unversioned() -> Self {
        Self {
            schema_version: DIAGNOSTICS_STATE_SCHEMA_VERSION,
            path_norm_version: DIAGNOSTICS_PATH_NORM_VERSION,
            normalized_paths: false,
            repo_root_id: None,
        }
    }
}

pub fn current_repo_root_id(source_root: Option<&Path>) -> Option<String> {
    let cwd = std::env::current_dir().ok();
    let root = source_root
        .map(|p| p.to_path_buf())
        .or_else(|| cwd.as_deref().and_then(git_toplevel_from))
        .or(cwd)?;

    let canonical = root.canonicalize().ok().unwrap_or(root);
    let root_str = canonical.to_string_lossy();
    Some(blake3::hash(root_str.as_bytes()).to_hex().to_string()[..16].to_string())
}

/// Serialize DiagnosticsData for storage.
pub fn serialize_diagnostics(data: &DiagnosticsData) -> Result<Vec<u8>, CovyError> {
    serialize_diagnostics_with_metadata(data, &DiagnosticsStateMetadata::unversioned())
}

pub fn serialize_diagnostics_with_metadata(
    data: &DiagnosticsData,
    metadata: &DiagnosticsStateMetadata,
) -> Result<Vec<u8>, CovyError> {
    let mut blocks: Vec<(String, Vec<u8>)> = Vec::with_capacity(data.issues_by_file.len());
    for (path, issues) in &data.issues_by_file {
        let stored: Vec<StoredIssue> = issues.iter().map(stored_issue_from_runtime).collect();
        let bytes = bincode::serialize(&stored)
            .map_err(|e| CovyError::Cache(format!("Failed to serialize diagnostics block: {e}")))?;
        blocks.push((path.clone(), bytes));
    }

    let repo_root_bytes = metadata
        .repo_root_id
        .as_ref()
        .map(|s| s.as_bytes())
        .unwrap_or_default();

    let header_len = DIAGNOSTICS_MAGIC.len() + 2 + 2 + 1 + 8 + 1 + 4 + repo_root_bytes.len() + 4;

    let mut index_len = 0usize;
    for (path, _) in &blocks {
        index_len += 4 + path.len() + 8 + 4;
    }

    let payload_start = header_len + index_len;
    let payload_len: usize = blocks.iter().map(|(_, b)| b.len()).sum();
    let mut out = Vec::with_capacity(payload_start + payload_len);

    out.extend_from_slice(DIAGNOSTICS_MAGIC);
    out.extend_from_slice(&metadata.schema_version.to_le_bytes());
    out.extend_from_slice(&metadata.path_norm_version.to_le_bytes());
    out.push(if metadata.normalized_paths { 1 } else { 0 });
    out.extend_from_slice(&data.timestamp.to_le_bytes());
    out.push(match data.format {
        Some(DiagnosticsFormat::Sarif) => 1,
        None => 0,
    });
    out.extend_from_slice(&(repo_root_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(repo_root_bytes);
    out.extend_from_slice(&(blocks.len() as u32).to_le_bytes());

    let mut offset = 0u64;
    for (path, block) in &blocks {
        let path_bytes = path.as_bytes();
        out.extend_from_slice(&(path_bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(path_bytes);
        out.extend_from_slice(&offset.to_le_bytes());
        out.extend_from_slice(&(block.len() as u32).to_le_bytes());
        offset += block.len() as u64;
    }

    for (_, block) in blocks {
        out.extend_from_slice(&block);
    }

    debug_assert_eq!(out.len(), payload_start + payload_len);
    Ok(out)
}

/// Deserialize DiagnosticsData from bytes.
pub fn deserialize_diagnostics(data: &[u8]) -> Result<DiagnosticsData, CovyError> {
    deserialize_diagnostics_with_metadata(data).map(|(d, _)| d)
}

pub fn deserialize_diagnostics_with_metadata(
    data: &[u8],
) -> Result<(DiagnosticsData, Option<DiagnosticsStateMetadata>), CovyError> {
    if is_new_diagnostics_format(data) {
        let state = parse_diagnostics_state(data)?;
        let diag = load_all_from_state(data, &state)?;
        return Ok((diag, Some(state.meta)));
    }

    // Legacy fallback
    let stored: StoredDiagnosticsData = bincode::deserialize(data)
        .map_err(|e| CovyError::Cache(format!("Failed to deserialize diagnostics: {e}")))?;
    Ok((stored.into_runtime(), None))
}

pub fn deserialize_diagnostics_for_paths(
    data: &[u8],
    paths: &HashSet<String>,
) -> Result<(DiagnosticsData, Option<DiagnosticsStateMetadata>), CovyError> {
    if is_new_diagnostics_format(data) {
        let state = parse_diagnostics_state(data)?;
        let diag = load_selected_from_state(data, &state, paths)?;
        return Ok((diag, Some(state.meta)));
    }

    let (mut all, meta) = deserialize_diagnostics_with_metadata(data)?;
    all.issues_by_file.retain(|path, _| paths.contains(path));
    Ok((all, meta))
}

pub fn deserialize_diagnostics_for_paths_from_file(
    path: &Path,
    paths: &HashSet<String>,
) -> Result<(DiagnosticsData, Option<DiagnosticsStateMetadata>), CovyError> {
    let mut file = File::open(path)?;
    if !is_new_diagnostics_format_file(&mut file)? {
        let bytes = std::fs::read(path)?;
        return deserialize_diagnostics_for_paths(&bytes, paths);
    }

    file.seek(SeekFrom::Start(0))?;
    let state = parse_diagnostics_state_from_reader(&mut file)?;
    let meta = state.meta.clone();
    let diagnostics = load_selected_from_reader(&mut file, &state, paths)?;
    Ok((diagnostics, Some(meta)))
}

fn is_new_diagnostics_format(data: &[u8]) -> bool {
    data.len() >= DIAGNOSTICS_MAGIC.len() && &data[..DIAGNOSTICS_MAGIC.len()] == DIAGNOSTICS_MAGIC
}

fn is_new_diagnostics_format_file(file: &mut File) -> Result<bool, CovyError> {
    let mut magic = [0u8; 9];
    match file.read_exact(&mut magic) {
        Ok(()) => Ok(&magic == DIAGNOSTICS_MAGIC),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(e.into()),
    }
}

#[derive(Debug)]
struct DiagnosticsState {
    meta: DiagnosticsStateMetadata,
    timestamp: u64,
    format: Option<DiagnosticsFormat>,
    entries: Vec<DiagnosticsIndexEntry>,
    payload_start: usize,
}

#[derive(Debug)]
struct DiagnosticsIndexEntry {
    path: String,
    offset: u64,
    len: u32,
}

fn parse_diagnostics_state(data: &[u8]) -> Result<DiagnosticsState, CovyError> {
    let mut pos = 0usize;

    let read_u8 = |buf: &[u8], pos: &mut usize| -> Result<u8, CovyError> {
        if *pos + 1 > buf.len() {
            return Err(CovyError::Cache(
                "unexpected EOF while reading u8".to_string(),
            ));
        }
        let v = buf[*pos];
        *pos += 1;
        Ok(v)
    };

    let read_u16 = |buf: &[u8], pos: &mut usize| -> Result<u16, CovyError> {
        if *pos + 2 > buf.len() {
            return Err(CovyError::Cache(
                "unexpected EOF while reading u16".to_string(),
            ));
        }
        let v = u16::from_le_bytes(buf[*pos..*pos + 2].try_into().unwrap());
        *pos += 2;
        Ok(v)
    };

    let read_u32 = |buf: &[u8], pos: &mut usize| -> Result<u32, CovyError> {
        if *pos + 4 > buf.len() {
            return Err(CovyError::Cache(
                "unexpected EOF while reading u32".to_string(),
            ));
        }
        let v = u32::from_le_bytes(buf[*pos..*pos + 4].try_into().unwrap());
        *pos += 4;
        Ok(v)
    };

    let read_u64 = |buf: &[u8], pos: &mut usize| -> Result<u64, CovyError> {
        if *pos + 8 > buf.len() {
            return Err(CovyError::Cache(
                "unexpected EOF while reading u64".to_string(),
            ));
        }
        let v = u64::from_le_bytes(buf[*pos..*pos + 8].try_into().unwrap());
        *pos += 8;
        Ok(v)
    };

    if !is_new_diagnostics_format(data) {
        return Err(CovyError::Cache(
            "invalid diagnostics state magic".to_string(),
        ));
    }
    pos += DIAGNOSTICS_MAGIC.len();

    let schema_version = read_u16(data, &mut pos)?;
    let path_norm_version = read_u16(data, &mut pos)?;
    let normalized_paths = read_u8(data, &mut pos)? != 0;
    let timestamp = read_u64(data, &mut pos)?;
    let format = match read_u8(data, &mut pos)? {
        1 => Some(DiagnosticsFormat::Sarif),
        _ => None,
    };

    let repo_root_len = read_u32(data, &mut pos)? as usize;
    if pos + repo_root_len > data.len() {
        return Err(CovyError::Cache(
            "unexpected EOF while reading repo root id".to_string(),
        ));
    }
    let repo_root_id = if repo_root_len > 0 {
        Some(String::from_utf8_lossy(&data[pos..pos + repo_root_len]).to_string())
    } else {
        None
    };
    pos += repo_root_len;

    let file_count = read_u32(data, &mut pos)? as usize;
    let mut entries = Vec::with_capacity(file_count);
    for _ in 0..file_count {
        let path_len = read_u32(data, &mut pos)? as usize;
        if pos + path_len > data.len() {
            return Err(CovyError::Cache(
                "unexpected EOF while reading diagnostics path".to_string(),
            ));
        }
        let path = String::from_utf8_lossy(&data[pos..pos + path_len]).to_string();
        pos += path_len;

        let offset = read_u64(data, &mut pos)?;
        let len = read_u32(data, &mut pos)?;

        entries.push(DiagnosticsIndexEntry { path, offset, len });
    }

    Ok(DiagnosticsState {
        meta: DiagnosticsStateMetadata {
            schema_version,
            path_norm_version,
            normalized_paths,
            repo_root_id,
        },
        timestamp,
        format,
        entries,
        payload_start: pos,
    })
}

fn load_all_from_state(
    data: &[u8],
    state: &DiagnosticsState,
) -> Result<DiagnosticsData, CovyError> {
    let mut issues_by_file = std::collections::BTreeMap::new();
    for entry in &state.entries {
        let issues = decode_issues_block(data, state.payload_start, entry)?;
        if !issues.is_empty() {
            issues_by_file.insert(entry.path.clone(), issues);
        }
    }
    Ok(DiagnosticsData {
        issues_by_file,
        format: state.format,
        timestamp: state.timestamp,
    })
}

fn load_selected_from_state(
    data: &[u8],
    state: &DiagnosticsState,
    selected_paths: &HashSet<String>,
) -> Result<DiagnosticsData, CovyError> {
    let mut issues_by_file = std::collections::BTreeMap::new();
    if selected_paths.is_empty() {
        return Ok(DiagnosticsData {
            issues_by_file,
            format: state.format,
            timestamp: state.timestamp,
        });
    }

    for entry in &state.entries {
        if !selected_paths.contains(&entry.path) {
            continue;
        }
        let issues = decode_issues_block(data, state.payload_start, entry)?;
        if !issues.is_empty() {
            issues_by_file.insert(entry.path.clone(), issues);
        }
    }

    Ok(DiagnosticsData {
        issues_by_file,
        format: state.format,
        timestamp: state.timestamp,
    })
}

fn load_selected_from_reader(
    file: &mut File,
    state: &DiagnosticsState,
    selected_paths: &HashSet<String>,
) -> Result<DiagnosticsData, CovyError> {
    let mut issues_by_file = std::collections::BTreeMap::new();
    if selected_paths.is_empty() {
        return Ok(DiagnosticsData {
            issues_by_file,
            format: state.format,
            timestamp: state.timestamp,
        });
    }

    for entry in &state.entries {
        if !selected_paths.contains(&entry.path) {
            continue;
        }
        let issues = decode_issues_block_from_reader(file, state.payload_start, entry)?;
        if !issues.is_empty() {
            issues_by_file.insert(entry.path.clone(), issues);
        }
    }

    Ok(DiagnosticsData {
        issues_by_file,
        format: state.format,
        timestamp: state.timestamp,
    })
}

fn decode_issues_block(
    data: &[u8],
    payload_start: usize,
    entry: &DiagnosticsIndexEntry,
) -> Result<Vec<Issue>, CovyError> {
    let start = payload_start
        .checked_add(entry.offset as usize)
        .ok_or_else(|| CovyError::Cache("diagnostics block offset overflow".to_string()))?;
    let end = start
        .checked_add(entry.len as usize)
        .ok_or_else(|| CovyError::Cache("diagnostics block length overflow".to_string()))?;

    if end > data.len() {
        return Err(CovyError::Cache(
            "diagnostics block exceeds file length".to_string(),
        ));
    }

    let stored: Vec<StoredIssue> = bincode::deserialize(&data[start..end])
        .map_err(|e| CovyError::Cache(format!("Failed to deserialize diagnostics block: {e}")))?;

    Ok(stored.into_iter().map(runtime_issue_from_stored).collect())
}

fn decode_issues_block_from_reader(
    file: &mut File,
    payload_start: usize,
    entry: &DiagnosticsIndexEntry,
) -> Result<Vec<Issue>, CovyError> {
    let start = payload_start
        .checked_add(entry.offset as usize)
        .ok_or_else(|| CovyError::Cache("diagnostics block offset overflow".to_string()))?;
    file.seek(SeekFrom::Start(start as u64))?;

    let mut block = vec![0u8; entry.len as usize];
    file.read_exact(&mut block)?;

    let stored: Vec<StoredIssue> = bincode::deserialize(&block)
        .map_err(|e| CovyError::Cache(format!("Failed to deserialize diagnostics block: {e}")))?;
    Ok(stored.into_iter().map(runtime_issue_from_stored).collect())
}

fn stored_issue_from_runtime(issue: &Issue) -> StoredIssue {
    StoredIssue {
        path: issue.path.clone(),
        line: issue.line,
        column: issue.column,
        end_line: issue.end_line,
        severity: issue.severity,
        rule_id: issue.rule_id.clone(),
        message: issue.message.clone(),
        source: issue.source.clone(),
        fingerprint: issue.fingerprint.clone(),
    }
}

fn runtime_issue_from_stored(issue: StoredIssue) -> Issue {
    Issue {
        path: issue.path,
        line: issue.line,
        column: issue.column,
        end_line: issue.end_line,
        severity: issue.severity,
        rule_id: issue.rule_id,
        message: issue.message,
        source: issue.source,
        fingerprint: issue.fingerprint,
    }
}

fn parse_diagnostics_state_from_reader(file: &mut File) -> Result<DiagnosticsState, CovyError> {
    let mut magic = [0u8; 9];
    file.read_exact(&mut magic)?;
    if &magic != DIAGNOSTICS_MAGIC {
        return Err(CovyError::Cache(
            "invalid diagnostics state magic".to_string(),
        ));
    }

    let schema_version = read_u16(file)?;
    let path_norm_version = read_u16(file)?;
    let normalized_paths = read_u8(file)? != 0;
    let timestamp = read_u64(file)?;
    let format = match read_u8(file)? {
        1 => Some(DiagnosticsFormat::Sarif),
        _ => None,
    };

    let repo_root_len = read_u32(file)? as usize;
    let mut repo_root_bytes = vec![0u8; repo_root_len];
    if repo_root_len > 0 {
        file.read_exact(&mut repo_root_bytes)?;
    }
    let repo_root_id = if repo_root_len > 0 {
        Some(String::from_utf8_lossy(&repo_root_bytes).to_string())
    } else {
        None
    };

    let file_count = read_u32(file)? as usize;
    let mut entries = Vec::with_capacity(file_count);
    for _ in 0..file_count {
        let path_len = read_u32(file)? as usize;
        let mut path_bytes = vec![0u8; path_len];
        if path_len > 0 {
            file.read_exact(&mut path_bytes)?;
        }
        let path = String::from_utf8_lossy(&path_bytes).to_string();
        let offset = read_u64(file)?;
        let len = read_u32(file)?;
        entries.push(DiagnosticsIndexEntry { path, offset, len });
    }

    let payload_start = file.stream_position()? as usize;
    Ok(DiagnosticsState {
        meta: DiagnosticsStateMetadata {
            schema_version,
            path_norm_version,
            normalized_paths,
            repo_root_id,
        },
        timestamp,
        format,
        entries,
        payload_start,
    })
}

fn read_u8(file: &mut File) -> Result<u8, CovyError> {
    let mut buf = [0u8; 1];
    file.read_exact(&mut buf)?;
    Ok(buf[0])
}

fn read_u16(file: &mut File) -> Result<u16, CovyError> {
    let mut buf = [0u8; 2];
    file.read_exact(&mut buf)?;
    Ok(u16::from_le_bytes(buf))
}

fn read_u32(file: &mut File) -> Result<u32, CovyError> {
    let mut buf = [0u8; 4];
    file.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

fn read_u64(file: &mut File) -> Result<u64, CovyError> {
    let mut buf = [0u8; 8];
    file.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

fn git_toplevel_from(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
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
    fn into_runtime(self) -> DiagnosticsData {
        let mut issues_by_file = std::collections::BTreeMap::new();
        for (path, issues) in self.issues_by_file {
            let runtime: Vec<Issue> = issues.into_iter().map(runtime_issue_from_stored).collect();
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

        let bytes = serialize_diagnostics_with_metadata(
            &data,
            &DiagnosticsStateMetadata::normalized_for_repo_root(Some("abc".to_string())),
        )
        .unwrap();

        let (restored, meta) = deserialize_diagnostics_with_metadata(&bytes).unwrap();
        assert_eq!(restored.total_issues(), 1);
        assert_eq!(restored.issues_by_file["src/main.rs"][0].rule_id, "R001");
        let meta = meta.unwrap();
        assert_eq!(meta.schema_version, DIAGNOSTICS_STATE_SCHEMA_VERSION);
        assert!(meta.normalized_paths);
        assert_eq!(meta.repo_root_id.as_deref(), Some("abc"));
    }

    #[test]
    fn test_diagnostics_selective_deserialize() {
        let mut data = DiagnosticsData::new();
        data.issues_by_file.insert(
            "src/a.rs".to_string(),
            vec![crate::diagnostics::Issue {
                path: "src/a.rs".to_string(),
                line: 1,
                column: None,
                end_line: None,
                severity: crate::diagnostics::Severity::Warning,
                rule_id: "A".to_string(),
                message: "a".to_string(),
                source: "tool".to_string(),
                fingerprint: "fpa".to_string(),
            }],
        );
        data.issues_by_file.insert(
            "src/b.rs".to_string(),
            vec![crate::diagnostics::Issue {
                path: "src/b.rs".to_string(),
                line: 2,
                column: None,
                end_line: None,
                severity: crate::diagnostics::Severity::Error,
                rule_id: "B".to_string(),
                message: "b".to_string(),
                source: "tool".to_string(),
                fingerprint: "fpb".to_string(),
            }],
        );

        let bytes = serialize_diagnostics_with_metadata(
            &data,
            &DiagnosticsStateMetadata::normalized_for_repo_root(None),
        )
        .unwrap();

        let mut selected = HashSet::new();
        selected.insert("src/b.rs".to_string());
        let (restored, _) = deserialize_diagnostics_for_paths(&bytes, &selected).unwrap();
        assert_eq!(restored.total_issues(), 1);
        assert!(restored.issues_by_file.contains_key("src/b.rs"));
        assert!(!restored.issues_by_file.contains_key("src/a.rs"));
    }

    #[test]
    fn test_diagnostics_selective_deserialize_from_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("issues.bin");

        let mut data = DiagnosticsData::new();
        data.issues_by_file.insert(
            "src/a.rs".to_string(),
            vec![crate::diagnostics::Issue {
                path: "src/a.rs".to_string(),
                line: 1,
                column: None,
                end_line: None,
                severity: crate::diagnostics::Severity::Warning,
                rule_id: "A".to_string(),
                message: "a".to_string(),
                source: "tool".to_string(),
                fingerprint: "fpa".to_string(),
            }],
        );
        data.issues_by_file.insert(
            "src/b.rs".to_string(),
            vec![crate::diagnostics::Issue {
                path: "src/b.rs".to_string(),
                line: 2,
                column: None,
                end_line: None,
                severity: crate::diagnostics::Severity::Error,
                rule_id: "B".to_string(),
                message: "b".to_string(),
                source: "tool".to_string(),
                fingerprint: "fpb".to_string(),
            }],
        );

        let bytes = serialize_diagnostics_with_metadata(
            &data,
            &DiagnosticsStateMetadata::normalized_for_repo_root(None),
        )
        .unwrap();
        std::fs::write(&path, bytes).unwrap();

        let mut selected = HashSet::new();
        selected.insert("src/a.rs".to_string());

        let (restored, meta) =
            deserialize_diagnostics_for_paths_from_file(&path, &selected).unwrap();
        assert_eq!(restored.total_issues(), 1);
        assert!(restored.issues_by_file.contains_key("src/a.rs"));
        assert!(!restored.issues_by_file.contains_key("src/b.rs"));
        assert!(meta.is_some());
    }

    #[test]
    fn test_testmap_serialization_roundtrip() {
        let mut index = TestMapIndex::default();
        index
            .test_to_files
            .entry("com.foo.BarTest".to_string())
            .or_default()
            .insert("src/main/java/com/foo/Bar.java".to_string());
        index
            .file_to_tests
            .entry("src/main/java/com/foo/Bar.java".to_string())
            .or_default()
            .insert("com.foo.BarTest".to_string());

        let bytes = serialize_testmap(&index).unwrap();
        let restored = deserialize_testmap(&bytes).unwrap();
        assert_eq!(restored.metadata.schema_version, TESTMAP_SCHEMA_VERSION);
        assert_eq!(restored.test_to_files.len(), 1);
        assert_eq!(restored.file_to_tests.len(), 1);
    }

    #[test]
    fn test_testmap_deserialize_legacy_v1_payload() {
        let legacy = LegacyTestMapIndexV1 {
            metadata: LegacyTestMapMetadataV1 {
                schema_version: 1,
                path_norm_version: 1,
                repo_root_id: Some("deadbeef".to_string()),
                generated_at: 123,
                granularity: "file".to_string(),
            },
            test_language: {
                let mut m = std::collections::BTreeMap::new();
                m.insert("com.foo.BarTest".to_string(), "java".to_string());
                m
            },
            test_to_files: {
                let mut m = std::collections::BTreeMap::new();
                m.entry("com.foo.BarTest".to_string())
                    .or_insert_with(std::collections::BTreeSet::new)
                    .insert("src/main/java/com/foo/Bar.java".to_string());
                m
            },
            file_to_tests: {
                let mut m = std::collections::BTreeMap::new();
                m.entry("src/main/java/com/foo/Bar.java".to_string())
                    .or_insert_with(std::collections::BTreeSet::new)
                    .insert("com.foo.BarTest".to_string());
                m
            },
        };

        let bytes = bincode::serialize(&legacy).unwrap();
        let restored = deserialize_testmap(&bytes).unwrap();
        assert_eq!(restored.metadata.schema_version, 1);
        assert_eq!(
            restored
                .test_to_files
                .get("com.foo.BarTest")
                .map(|s| s.len())
                .unwrap_or_default(),
            1
        );
        assert!(restored.tests.is_empty());
        assert!(restored.coverage.is_empty());
    }

    #[test]
    fn test_testmap_deserialize_struct_v1_payload_is_normalized() {
        let mut index = TestMapIndex::default();
        index.metadata.schema_version = 1;
        index.metadata.path_norm_version = 1;
        index.metadata.repo_root_id = Some("deadbeef".to_string());
        index.metadata.generated_at = 123;
        index.metadata.granularity = "file".to_string();
        index.metadata.commit_sha = Some("abc123".to_string());
        index.metadata.created_at = Some(321);
        index.metadata.toolchain_fingerprint = Some("toolchain".to_string());
        index
            .test_to_files
            .entry("com.foo.BarTest".to_string())
            .or_default()
            .insert("src/main/java/com/foo/Bar.java".to_string());
        index
            .file_to_tests
            .entry("src/main/java/com/foo/Bar.java".to_string())
            .or_default()
            .insert("com.foo.BarTest".to_string());
        index.tests.push("com.foo.BarTest".to_string());
        index
            .file_index
            .push("src/main/java/com/foo/Bar.java".to_string());
        index.coverage = vec![vec![vec![10]]];

        let bytes = bincode::serialize(&index).unwrap();
        let restored = deserialize_testmap(&bytes).unwrap();
        assert_eq!(restored.metadata.schema_version, 1);
        assert!(restored.metadata.commit_sha.is_none());
        assert!(restored.metadata.created_at.is_none());
        assert!(restored.metadata.toolchain_fingerprint.is_none());
        assert!(restored.tests.is_empty());
        assert!(restored.file_index.is_empty());
        assert!(restored.coverage.is_empty());
        assert_eq!(restored.test_to_files.len(), 1);
        assert_eq!(restored.file_to_tests.len(), 1);
    }

    #[test]
    fn test_testtimings_serialization_roundtrip() {
        let mut timings = TestTimingHistory::default();
        timings.duration_ms.insert("test_a".to_string(), 1200);
        timings.sample_count.insert("test_a".to_string(), 3);
        timings.last_seen.insert("test_a".to_string(), 100);

        let bytes = serialize_test_timings(&timings).unwrap();
        let restored = deserialize_test_timings(&bytes).unwrap();
        assert_eq!(restored.duration_ms.get("test_a"), Some(&1200));
        assert_eq!(restored.sample_count.get("test_a"), Some(&3));
    }
}
