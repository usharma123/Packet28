use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::PacketCache;

pub const DEFAULT_PERSIST_TTL_SECS: u64 = 86_400;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum EvictionReason {
    ExpiredTtl,
    ManualPrune,
    VersionMismatch,
    CorruptLoadRecovery,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct EvictionCounters {
    pub expired_ttl: usize,
    pub manual_prune: usize,
    pub version_mismatch: usize,
    pub corrupt_load_recovery: usize,
}

impl EvictionCounters {
    pub(crate) fn add(&mut self, reason: EvictionReason, count: usize) {
        match reason {
            EvictionReason::ExpiredTtl => self.expired_ttl = self.expired_ttl.saturating_add(count),
            EvictionReason::ManualPrune => {
                self.manual_prune = self.manual_prune.saturating_add(count)
            }
            EvictionReason::VersionMismatch => {
                self.version_mismatch = self.version_mismatch.saturating_add(count)
            }
            EvictionReason::CorruptLoadRecovery => {
                self.corrupt_load_recovery = self.corrupt_load_recovery.saturating_add(count)
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ContextStoreListFilter {
    pub target: Option<String>,
    pub contains_query: Option<String>,
    pub created_after_unix: Option<u64>,
    pub created_before_unix: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct ContextStorePaging {
    pub offset: usize,
    pub limit: usize,
}

impl Default for ContextStorePaging {
    fn default() -> Self {
        Self {
            offset: 0,
            limit: 100,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ContextStoreEntrySummary {
    pub cache_key: String,
    pub target: String,
    pub input_hash: String,
    pub created_at_unix: u64,
    pub age_secs: u64,
    pub packet_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextStoreEntryDetail {
    pub entry: PacketCacheEntry,
    pub age_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ContextStoreStats {
    pub entries: usize,
    pub oldest_created_at_unix: Option<u64>,
    pub newest_created_at_unix: Option<u64>,
    pub evictions: EvictionCounters,
}

#[derive(Debug, Clone, Default)]
pub struct ContextStorePruneRequest {
    pub all: bool,
    pub ttl_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ContextStorePruneReport {
    pub removed: usize,
    pub remaining: usize,
    pub reasons: EvictionCounters,
}

#[derive(Debug, Clone)]
pub struct RecallOptions {
    pub limit: usize,
    pub since_unix: Option<u64>,
    pub until_unix: Option<u64>,
    pub target: Option<String>,
    pub task_id: Option<String>,
    pub scope: RecallScope,
    pub packet_types: Vec<String>,
    pub path_filters: Vec<String>,
    pub symbol_filters: Vec<String>,
}

impl Default for RecallOptions {
    fn default() -> Self {
        Self {
            limit: 8,
            since_unix: None,
            until_unix: None,
            target: None,
            task_id: None,
            scope: RecallScope::Global,
            packet_types: Vec::new(),
            path_filters: Vec::new(),
            symbol_filters: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecallScope {
    #[default]
    Global,
    TaskFirst,
    TaskOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RecallBudgetEstimate {
    pub est_tokens: u64,
    pub est_bytes: u64,
    pub runtime_ms: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecallSourceTier {
    CuratedMemory,
    Telemetry,
    #[default]
    Standard,
}

impl RecallSourceTier {
    pub fn as_str(self) -> &'static str {
        match self {
            RecallSourceTier::CuratedMemory => "curated_memory",
            RecallSourceTier::Telemetry => "telemetry",
            RecallSourceTier::Standard => "standard",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RecallHit {
    pub cache_key: String,
    pub target: String,
    pub created_at_unix: u64,
    pub age_secs: u64,
    pub score: f64,
    pub summary: Option<String>,
    pub snippet: String,
    pub matched_tokens: Vec<String>,
    pub matched_paths: Vec<String>,
    pub matched_symbols: Vec<String>,
    pub match_reasons: Vec<String>,
    pub packet_types: Vec<String>,
    pub task_ids: Vec<String>,
    pub budget_estimate: RecallBudgetEstimate,
    pub source_tier: RecallSourceTier,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct NormalizedPathRef {
    pub canonical: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub basename: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RelatedEntryMatch {
    pub entry: PacketCacheEntry,
    pub canonical_path_matches: Vec<String>,
    pub basename_path_matches: Vec<String>,
    pub symbol_matches: Vec<String>,
    pub test_matches: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct PersistConfig {
    pub root_dir: PathBuf,
    pub ttl_secs: u64,
}

impl PersistConfig {
    pub fn new(root_dir: PathBuf) -> Self {
        Self {
            root_dir,
            ttl_secs: DEFAULT_PERSIST_TTL_SECS,
        }
    }

    pub fn with_ttl_secs(mut self, ttl_secs: u64) -> Self {
        self.ttl_secs = ttl_secs;
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CachePacket {
    pub packet_id: Option<String>,
    pub body: Value,
    pub token_usage: Option<u64>,
    pub runtime_ms: Option<u64>,
    pub metadata: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DeltaReuse {
    pub reused_from: Option<String>,
    pub delta_ratio: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PacketCacheEntry {
    pub cache_key: String,
    pub target: String,
    pub input_hash: String,
    pub created_at_unix: u64,
    pub packets: Vec<CachePacket>,
    pub metadata: Value,
    pub delta_reuse: DeltaReuse,
}

#[derive(Debug, Clone)]
pub struct CacheLookup {
    pub cache_key: String,
    pub input_hash: String,
    pub entry: Option<PacketCacheEntry>,
    pub suggested_reuse_base: Option<String>,
}

pub trait DeltaReuseHooks {
    fn select_reuse_base(
        &mut self,
        _target: &str,
        _input_hash: &str,
        _cache: &PacketCache,
    ) -> Option<String> {
        None
    }

    fn on_hit(&mut self, _entry: &PacketCacheEntry) {}

    fn on_put(&mut self, _entry: &PacketCacheEntry) {}
}

#[derive(Default)]
pub struct NoopDeltaReuseHooks;

impl DeltaReuseHooks for NoopDeltaReuseHooks {}
