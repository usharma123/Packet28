use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const DEFAULT_PERSIST_TTL_SECS: u64 = 86_400;
const PERSIST_CACHE_VERSION: u32 = 2;
const PERSIST_CACHE_DIR: &str = ".packet28";
const PERSIST_CACHE_FILE_V1: &str = "packet-cache-v1.bin";
const PERSIST_CACHE_FILE_V2: &str = "packet-cache-v2.bin";

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
    fn add(&mut self, reason: EvictionReason, count: usize) {
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
struct PersistEnvelopeV1 {
    version: u32,
    entries: Vec<PersistPacketCacheEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct PersistEnvelopeV2 {
    version: u32,
    entries: Vec<PersistPacketCacheEntry>,
    recall_docs: Vec<RecallDocument>,
    recall_postings: HashMap<String, Vec<(String, usize)>>,
    recall_avg_doc_length: f64,
    file_ref_index: HashMap<String, BTreeSet<String>>,
    basename_alias_index: HashMap<String, BTreeSet<String>>,
    symbol_index: HashMap<String, BTreeSet<String>>,
    test_index: HashMap<String, BTreeSet<String>>,
    task_index: HashMap<String, BTreeSet<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PersistPacketCacheEntry {
    cache_key: String,
    target: String,
    input_hash: String,
    created_at_unix: u64,
    packets: Vec<PersistCachePacket>,
    metadata_json: String,
    delta_reuse: DeltaReuse,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PersistCachePacket {
    packet_id: Option<String>,
    body_json: String,
    token_usage: Option<u64>,
    runtime_ms: Option<u64>,
    metadata_json: String,
}

impl PersistPacketCacheEntry {
    fn from_entry(entry: &PacketCacheEntry) -> Self {
        Self {
            cache_key: entry.cache_key.clone(),
            target: entry.target.clone(),
            input_hash: entry.input_hash.clone(),
            created_at_unix: entry.created_at_unix,
            packets: entry
                .packets
                .iter()
                .map(PersistCachePacket::from_cache_packet)
                .collect(),
            metadata_json: encode_json_value(&entry.metadata),
            delta_reuse: entry.delta_reuse.clone(),
        }
    }

    fn into_entry(self) -> PacketCacheEntry {
        PacketCacheEntry {
            cache_key: self.cache_key,
            target: self.target,
            input_hash: self.input_hash,
            created_at_unix: self.created_at_unix,
            packets: self
                .packets
                .into_iter()
                .map(PersistCachePacket::into_cache_packet)
                .collect(),
            metadata: decode_json_value(&self.metadata_json),
            delta_reuse: self.delta_reuse,
        }
    }
}

impl PersistCachePacket {
    fn from_cache_packet(packet: &CachePacket) -> Self {
        Self {
            packet_id: packet.packet_id.clone(),
            body_json: encode_json_value(&packet.body),
            token_usage: packet.token_usage,
            runtime_ms: packet.runtime_ms,
            metadata_json: encode_json_value(&packet.metadata),
        }
    }

    fn into_cache_packet(self) -> CachePacket {
        CachePacket {
            packet_id: self.packet_id,
            body: decode_json_value(&self.body_json),
            token_usage: self.token_usage,
            runtime_ms: self.runtime_ms,
            metadata: decode_json_value(&self.metadata_json),
        }
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

#[derive(Default)]
pub struct PacketCache {
    entries_by_hash: HashMap<String, PacketCacheEntry>,
    latest_request_index: HashMap<String, String>,
    eviction_counters: EvictionCounters,
    workspace_root: Option<PathBuf>,
    recall_docs: HashMap<String, RecallDocument>,
    recall_postings: HashMap<String, Vec<(String, usize)>>,
    recall_avg_doc_length: f64,
    recall_total_doc_length: usize,
    file_ref_index: HashMap<String, BTreeSet<String>>,
    basename_alias_index: HashMap<String, BTreeSet<String>>,
    symbol_index: HashMap<String, BTreeSet<String>>,
    test_index: HashMap<String, BTreeSet<String>>,
    task_index: HashMap<String, BTreeSet<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct RecallDocument {
    cache_key: String,
    target: String,
    created_at_unix: u64,
    summary: Option<String>,
    snippet: String,
    task_ids: Vec<String>,
    packet_types: Vec<String>,
    paths: Vec<String>,
    path_basenames: Vec<String>,
    symbols: Vec<String>,
    tests: Vec<String>,
    terms: HashMap<String, usize>,
    doc_length: usize,
    budget_estimate: RecallBudgetEstimate,
}

impl PacketCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn load_from_disk(config: &PersistConfig) -> Self {
        let mut cache = Self {
            workspace_root: Some(config.root_dir.clone()),
            ..Self::new()
        };
        if cache.try_load_v2(config).is_none() {
            let _ = cache.try_load_v1(config);
        }
        cache.rebuild_latest_request_index();
        cache.evict_expired(config.ttl_secs);
        if cache.recall_docs.is_empty() && !cache.entries_by_hash.is_empty() {
            cache.rebuild_indexes();
        }
        cache
    }

    pub fn save_to_disk(&self, config: &PersistConfig) -> Result<(), io::Error> {
        let path = persist_cache_path_v2(&config.root_dir);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let live_entries = self.collect_live_entries(config.ttl_secs);
        let live_keys = live_entries
            .iter()
            .map(|entry| entry.cache_key.clone())
            .collect::<BTreeSet<_>>();
        let envelope = PersistEnvelopeV2 {
            version: PERSIST_CACHE_VERSION,
            entries: live_entries,
            recall_docs: self
                .recall_docs
                .iter()
                .filter(|(cache_key, _)| live_keys.contains(*cache_key))
                .map(|(_, doc)| doc.clone())
                .collect(),
            recall_postings: filter_postings_for_live_keys(&self.recall_postings, &live_keys),
            recall_avg_doc_length: self.recall_avg_doc_length,
            file_ref_index: filter_ref_index_for_live_keys(&self.file_ref_index, &live_keys),
            basename_alias_index: self.basename_alias_index.clone(),
            symbol_index: filter_ref_index_for_live_keys(&self.symbol_index, &live_keys),
            test_index: filter_ref_index_for_live_keys(&self.test_index, &live_keys),
            task_index: filter_ref_index_for_live_keys(&self.task_index, &live_keys),
        };

        let encoded = bincode::serialize(&envelope).map_err(|source| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("failed to serialize cache envelope: {source}"),
            )
        })?;

        write_atomically(&path, &encoded)
    }

    pub fn evict_expired(&mut self, ttl_secs: u64) {
        let removed = self.remove_where(
            |entry, now| is_expired(entry.created_at_unix, ttl_secs, now),
            EvictionReason::ExpiredTtl,
        );
        if removed > 0 {
            self.evict_reason(EvictionReason::ExpiredTtl, removed);
        }
    }

    pub fn len(&self) -> usize {
        self.entries_by_hash.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries_by_hash.is_empty()
    }

    pub fn hash_value(value: &Value) -> String {
        let bytes = serde_json::to_vec(value).unwrap_or_default();
        blake3::hash(&bytes).to_hex().to_string()
    }

    pub fn compute_input_hash(target: &str, reducer_input: &Value) -> String {
        let payload = serde_json::json!({
            "target": target,
            "reducer_input": reducer_input,
        });
        Self::hash_value(&payload)
    }

    pub fn compute_request_hash(target: &str, input_hash: &str) -> String {
        let mut material = String::with_capacity(target.len() + input_hash.len() + 3);
        material.push_str(target.trim());
        material.push(':');
        material.push(':');
        material.push_str(input_hash);
        blake3::hash(material.as_bytes()).to_hex().to_string()
    }

    pub fn get(&self, cache_key: &str) -> Option<&PacketCacheEntry> {
        self.entries_by_hash.get(cache_key)
    }

    pub fn get_by_request(&self, target: &str, input_hash: &str) -> Option<&PacketCacheEntry> {
        let request_key = Self::compute_request_hash(target, input_hash);
        self.latest_request_index
            .get(&request_key)
            .and_then(|cache_key| self.get(cache_key))
    }

    pub fn lookup_with_hooks(
        &self,
        target: &str,
        reducer_input: &Value,
        hooks: &mut dyn DeltaReuseHooks,
    ) -> CacheLookup {
        let input_hash = Self::compute_input_hash(target, reducer_input);
        let request_hash = Self::compute_request_hash(target, &input_hash);
        let entry = self
            .latest_request_index
            .get(&request_hash)
            .and_then(|cache_key| self.get(cache_key))
            .cloned();

        if let Some(hit) = entry.as_ref() {
            hooks.on_hit(hit);
        }

        let suggested_reuse_base = if entry.is_none() {
            hooks.select_reuse_base(target, &input_hash, self)
        } else {
            None
        };

        CacheLookup {
            cache_key: request_hash,
            input_hash,
            entry,
            suggested_reuse_base,
        }
    }

    pub fn put_with_hooks(
        &mut self,
        target: &str,
        lookup: &CacheLookup,
        packets: Vec<CachePacket>,
        metadata: Value,
        hooks: &mut dyn DeltaReuseHooks,
    ) -> PacketCacheEntry {
        let entry = PacketCacheEntry {
            cache_key: lookup.cache_key.clone(),
            target: target.to_string(),
            input_hash: lookup.input_hash.clone(),
            created_at_unix: now_unix(),
            packets,
            metadata,
            delta_reuse: DeltaReuse {
                reused_from: lookup.suggested_reuse_base.clone(),
                delta_ratio: None,
            },
        };

        if self.entries_by_hash.contains_key(&entry.cache_key) {
            self.remove_index_for(&entry.cache_key);
        }
        self.entries_by_hash
            .insert(entry.cache_key.clone(), entry.clone());
        self.latest_request_index
            .insert(lookup.cache_key.clone(), entry.cache_key.clone());
        self.index_entry(&entry);
        hooks.on_put(&entry);
        entry
    }

    pub fn list_entries(
        &self,
        filter: &ContextStoreListFilter,
        paging: &ContextStorePaging,
    ) -> Vec<ContextStoreEntrySummary> {
        let now = now_unix();
        let target_filter = filter.target.as_ref().map(|v| v.to_ascii_lowercase());
        let contains_filter = filter
            .contains_query
            .as_ref()
            .map(|v| v.to_ascii_lowercase());

        let mut items = self
            .entries_by_hash
            .values()
            .filter(|entry| {
                if let Some(target) = target_filter.as_ref() {
                    if !entry.target.to_ascii_lowercase().contains(target) {
                        return false;
                    }
                }
                if let Some(contains) = contains_filter.as_ref() {
                    let haystack = format!(
                        "{} {} {}",
                        entry.cache_key.to_ascii_lowercase(),
                        entry.target.to_ascii_lowercase(),
                        entry.input_hash.to_ascii_lowercase()
                    );
                    if !haystack.contains(contains) {
                        return false;
                    }
                }
                if let Some(after) = filter.created_after_unix {
                    if entry.created_at_unix < after {
                        return false;
                    }
                }
                if let Some(before) = filter.created_before_unix {
                    if entry.created_at_unix > before {
                        return false;
                    }
                }
                true
            })
            .map(|entry| ContextStoreEntrySummary {
                cache_key: entry.cache_key.clone(),
                target: entry.target.clone(),
                input_hash: entry.input_hash.clone(),
                created_at_unix: entry.created_at_unix,
                age_secs: now.saturating_sub(entry.created_at_unix),
                packet_count: entry.packets.len(),
            })
            .collect::<Vec<_>>();

        items.sort_by(|a, b| {
            b.created_at_unix
                .cmp(&a.created_at_unix)
                .then_with(|| a.cache_key.cmp(&b.cache_key))
        });

        items
            .into_iter()
            .skip(paging.offset)
            .take(paging.limit.max(1))
            .collect()
    }

    pub fn get_entry(&self, cache_key: &str) -> Option<ContextStoreEntryDetail> {
        let now = now_unix();
        self.entries_by_hash
            .get(cache_key)
            .cloned()
            .map(|entry| ContextStoreEntryDetail {
                age_secs: now.saturating_sub(entry.created_at_unix),
                entry,
            })
    }

    pub fn entries(&self) -> Vec<PacketCacheEntry> {
        let mut entries = self.entries_by_hash.values().cloned().collect::<Vec<_>>();
        entries.sort_by(|a, b| {
            a.created_at_unix
                .cmp(&b.created_at_unix)
                .then_with(|| a.cache_key.cmp(&b.cache_key))
        });
        entries
    }

    pub fn related_entries(
        &self,
        task_id: Option<&str>,
        canonical_paths: &[String],
        symbols: &[String],
        tests: &[String],
    ) -> Vec<RelatedEntryMatch> {
        let task_filter = task_id.map(|value| value.to_ascii_lowercase());
        let task_keys = task_filter
            .as_ref()
            .and_then(|task_id| self.task_index.get(task_id))
            .cloned();
        let mut matches = HashMap::<String, RelatedEntryMatch>::new();

        for path in canonical_paths {
            if let Some(cache_keys) = self.file_ref_index.get(path) {
                for cache_key in cache_keys {
                    if !task_match_allowed(cache_key, task_keys.as_ref()) {
                        continue;
                    }
                    if let Some(entry) = self.entries_by_hash.get(cache_key).cloned() {
                        let item =
                            matches
                                .entry(cache_key.clone())
                                .or_insert_with(|| RelatedEntryMatch {
                                    entry,
                                    canonical_path_matches: Vec::new(),
                                    basename_path_matches: Vec::new(),
                                    symbol_matches: Vec::new(),
                                    test_matches: Vec::new(),
                                });
                        if !item
                            .canonical_path_matches
                            .iter()
                            .any(|existing| existing == path)
                        {
                            item.canonical_path_matches.push(path.clone());
                        }
                    }
                }
            } else if let Some(basename) = basename_alias(path) {
                if let Some(canonicals) = self.basename_alias_index.get(&basename) {
                    if canonicals.len() == 1 {
                        for canonical in canonicals {
                            if let Some(cache_keys) = self.file_ref_index.get(canonical) {
                                for cache_key in cache_keys {
                                    if !task_match_allowed(cache_key, task_keys.as_ref()) {
                                        continue;
                                    }
                                    if let Some(entry) =
                                        self.entries_by_hash.get(cache_key).cloned()
                                    {
                                        let item =
                                            matches.entry(cache_key.clone()).or_insert_with(|| {
                                                RelatedEntryMatch {
                                                    entry,
                                                    canonical_path_matches: Vec::new(),
                                                    basename_path_matches: Vec::new(),
                                                    symbol_matches: Vec::new(),
                                                    test_matches: Vec::new(),
                                                }
                                            });
                                        if !item
                                            .basename_path_matches
                                            .iter()
                                            .any(|existing| existing == canonical)
                                        {
                                            item.basename_path_matches.push(canonical.clone());
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        for symbol in symbols {
            let symbol = symbol.to_ascii_lowercase();
            for (candidate, cache_keys) in &self.symbol_index {
                if candidate == &symbol
                    || candidate.starts_with(&symbol)
                    || candidate.contains(&symbol)
                {
                    for cache_key in cache_keys {
                        if !task_match_allowed(cache_key, task_keys.as_ref()) {
                            continue;
                        }
                        if let Some(entry) = self.entries_by_hash.get(cache_key).cloned() {
                            let item = matches.entry(cache_key.clone()).or_insert_with(|| {
                                RelatedEntryMatch {
                                    entry,
                                    canonical_path_matches: Vec::new(),
                                    basename_path_matches: Vec::new(),
                                    symbol_matches: Vec::new(),
                                    test_matches: Vec::new(),
                                }
                            });
                            if !item
                                .symbol_matches
                                .iter()
                                .any(|existing| existing == candidate)
                            {
                                item.symbol_matches.push(candidate.clone());
                            }
                        }
                    }
                }
            }
        }

        for test in tests {
            let test = test.to_ascii_lowercase();
            for (candidate, cache_keys) in &self.test_index {
                if candidate == &test || candidate.starts_with(&test) || candidate.contains(&test) {
                    for cache_key in cache_keys {
                        if !task_match_allowed(cache_key, task_keys.as_ref()) {
                            continue;
                        }
                        if let Some(entry) = self.entries_by_hash.get(cache_key).cloned() {
                            let item = matches.entry(cache_key.clone()).or_insert_with(|| {
                                RelatedEntryMatch {
                                    entry,
                                    canonical_path_matches: Vec::new(),
                                    basename_path_matches: Vec::new(),
                                    symbol_matches: Vec::new(),
                                    test_matches: Vec::new(),
                                }
                            });
                            if !item
                                .test_matches
                                .iter()
                                .any(|existing| existing == candidate)
                            {
                                item.test_matches.push(candidate.clone());
                            }
                        }
                    }
                }
            }
        }

        let mut values = matches.into_values().collect::<Vec<_>>();
        values.sort_by(|a, b| a.entry.cache_key.cmp(&b.entry.cache_key));
        values
    }

    pub fn prune(&mut self, request: ContextStorePruneRequest) -> ContextStorePruneReport {
        let removed = if request.all {
            let removed = self.entries_by_hash.len();
            self.entries_by_hash.clear();
            self.latest_request_index.clear();
            self.clear_indexes();
            if removed > 0 {
                self.evict_reason(EvictionReason::ManualPrune, removed);
            }
            removed
        } else {
            let ttl_secs = request.ttl_secs.unwrap_or(DEFAULT_PERSIST_TTL_SECS);
            self.remove_where(
                |entry, now| is_expired(entry.created_at_unix, ttl_secs, now),
                EvictionReason::ManualPrune,
            )
        };

        ContextStorePruneReport {
            removed,
            remaining: self.entries_by_hash.len(),
            reasons: self.eviction_counters.clone(),
        }
    }

    pub fn stats(&self) -> ContextStoreStats {
        let oldest = self
            .entries_by_hash
            .values()
            .map(|v| v.created_at_unix)
            .min();
        let newest = self
            .entries_by_hash
            .values()
            .map(|v| v.created_at_unix)
            .max();
        ContextStoreStats {
            entries: self.entries_by_hash.len(),
            oldest_created_at_unix: oldest,
            newest_created_at_unix: newest,
            evictions: self.eviction_counters.clone(),
        }
    }

    pub fn recall(&self, query: &str, options: &RecallOptions) -> Vec<RecallHit> {
        let query_tokens = tokenize(query);
        if query_tokens.is_empty()
            && options.packet_types.is_empty()
            && options.path_filters.is_empty()
            && options.symbol_filters.is_empty()
        {
            return Vec::new();
        }

        let now = now_unix();
        let target_filter = options.target.as_ref().map(|v| v.to_ascii_lowercase());
        let packet_type_filters = options
            .packet_types
            .iter()
            .map(|item| item.to_ascii_lowercase())
            .collect::<Vec<_>>();
        let path_filters = options
            .path_filters
            .iter()
            .map(|item| item.to_ascii_lowercase())
            .collect::<Vec<_>>();
        let symbol_filters = options
            .symbol_filters
            .iter()
            .map(|item| item.to_ascii_lowercase())
            .collect::<Vec<_>>();
        let task_filter = options
            .task_id
            .as_ref()
            .map(|task| task.to_ascii_lowercase());
        let query_path_terms = extract_query_path_terms(query);
        let mut candidate_scores = HashMap::<String, f64>::new();
        let mut canonical_path_matches = HashMap::<String, BTreeSet<String>>::new();
        let mut basename_path_matches = HashMap::<String, BTreeSet<String>>::new();
        let mut symbol_index_matches = HashMap::<String, BTreeSet<String>>::new();
        let mut graph_overlap_candidates = BTreeSet::<String>::new();
        if query_tokens.is_empty() {
            for cache_key in self.recall_docs.keys() {
                candidate_scores.insert(cache_key.clone(), 0.0);
            }
        } else {
            for token in &query_tokens {
                if let Some(postings) = self.recall_postings.get(token) {
                    let idf = bm25_idf(self.recall_docs.len(), postings.len());
                    for (cache_key, tf) in postings {
                        if let Some(doc) = self.recall_docs.get(cache_key) {
                            let score = bm25_score(
                                *tf as f64,
                                doc.doc_length as f64,
                                self.recall_avg_doc_length.max(1.0),
                                idf,
                            );
                            *candidate_scores.entry(cache_key.clone()).or_insert(0.0) += score;
                        }
                    }
                }
            }
        }

        for needle in options
            .path_filters
            .iter()
            .chain(query_path_terms.iter())
            .cloned()
        {
            let normalized_needle = needle.to_ascii_lowercase();
            if let Some(path_ref) = normalize_context_path(&needle, self.workspace_root.as_deref())
            {
                if let Some(cache_keys) = self.file_ref_index.get(&path_ref.canonical) {
                    for cache_key in cache_keys {
                        candidate_scores.entry(cache_key.clone()).or_insert(0.0);
                        canonical_path_matches
                            .entry(cache_key.clone())
                            .or_default()
                            .insert(path_ref.canonical.clone());
                        graph_overlap_candidates.insert(cache_key.clone());
                    }
                } else if let Some(basename) = path_ref.basename.as_ref() {
                    if let Some(canonicals) = self.basename_alias_index.get(basename) {
                        if canonicals.len() == 1 {
                            for canonical in canonicals {
                                if let Some(cache_keys) = self.file_ref_index.get(canonical) {
                                    for cache_key in cache_keys {
                                        candidate_scores.entry(cache_key.clone()).or_insert(0.0);
                                        basename_path_matches
                                            .entry(cache_key.clone())
                                            .or_default()
                                            .insert(canonical.clone());
                                        graph_overlap_candidates.insert(cache_key.clone());
                                    }
                                }
                            }
                        }
                    }
                }
            }
            for (canonical, cache_keys) in &self.file_ref_index {
                if canonical.contains(&normalized_needle)
                    || canonical.starts_with(&normalized_needle)
                {
                    for cache_key in cache_keys {
                        candidate_scores.entry(cache_key.clone()).or_insert(0.0);
                        canonical_path_matches
                            .entry(cache_key.clone())
                            .or_default()
                            .insert(canonical.clone());
                        graph_overlap_candidates.insert(cache_key.clone());
                    }
                }
            }
            for (basename, canonicals) in &self.basename_alias_index {
                if (basename.contains(&normalized_needle)
                    || basename.starts_with(&normalized_needle))
                    && canonicals.len() == 1
                {
                    for canonical in canonicals {
                        if let Some(cache_keys) = self.file_ref_index.get(canonical) {
                            for cache_key in cache_keys {
                                candidate_scores.entry(cache_key.clone()).or_insert(0.0);
                                basename_path_matches
                                    .entry(cache_key.clone())
                                    .or_default()
                                    .insert(canonical.clone());
                                graph_overlap_candidates.insert(cache_key.clone());
                            }
                        }
                    }
                }
            }
        }

        for needle in options
            .symbol_filters
            .iter()
            .map(|item| item.to_ascii_lowercase())
            .chain(query_tokens.iter().cloned())
        {
            for (symbol, cache_keys) in &self.symbol_index {
                if symbol == &needle || symbol.starts_with(&needle) || symbol.contains(&needle) {
                    for cache_key in cache_keys {
                        candidate_scores.entry(cache_key.clone()).or_insert(0.0);
                        symbol_index_matches
                            .entry(cache_key.clone())
                            .or_default()
                            .insert(symbol.clone());
                        graph_overlap_candidates.insert(cache_key.clone());
                    }
                }
            }
        }

        for needle in &query_tokens {
            for (test_id, cache_keys) in &self.test_index {
                if test_id == needle || test_id.starts_with(needle) || test_id.contains(needle) {
                    for cache_key in cache_keys {
                        candidate_scores.entry(cache_key.clone()).or_insert(0.0);
                        graph_overlap_candidates.insert(cache_key.clone());
                    }
                }
            }
        }

        let mut hits = Vec::new();
        for (cache_key, base_score) in candidate_scores {
            let Some(doc) = self.recall_docs.get(&cache_key) else {
                continue;
            };
            if let Some(target) = target_filter.as_ref() {
                if !doc.target.to_ascii_lowercase().contains(target) {
                    continue;
                }
            }
            if let Some(since) = options.since_unix {
                if doc.created_at_unix < since {
                    continue;
                }
            }
            if let Some(until) = options.until_unix {
                if doc.created_at_unix > until {
                    continue;
                }
            }
            if !packet_type_filters.is_empty()
                && !packet_type_filters.iter().all(|needle| {
                    doc.packet_types
                        .iter()
                        .any(|packet_type| packet_type.contains(needle))
                })
            {
                continue;
            }
            match options.scope {
                RecallScope::Global => {}
                RecallScope::TaskFirst => {
                    if let Some(task_id) = task_filter.as_ref() {
                        if !doc.task_ids.iter().any(|item| item == task_id) {
                            // allowed, but task-local docs receive a boost below
                        }
                    }
                }
                RecallScope::TaskOnly => {
                    if let Some(task_id) = task_filter.as_ref() {
                        if !doc.task_ids.iter().any(|item| item == task_id) {
                            continue;
                        }
                    } else {
                        continue;
                    }
                }
            }

            let age_secs = now.saturating_sub(doc.created_at_unix);
            let mut matched_paths = canonical_path_matches
                .remove(&cache_key)
                .unwrap_or_default()
                .into_iter()
                .collect::<Vec<_>>();
            let basename_matches = basename_path_matches
                .remove(&cache_key)
                .unwrap_or_default()
                .into_iter()
                .collect::<Vec<_>>();
            let extra_path_matches = basename_matches
                .iter()
                .filter(|item| !matched_paths.iter().any(|existing| existing == *item))
                .cloned()
                .collect::<Vec<_>>();
            matched_paths.extend(extra_path_matches);
            let mut matched_symbols = symbol_index_matches
                .remove(&cache_key)
                .unwrap_or_default()
                .into_iter()
                .collect::<Vec<_>>();
            for item in collect_matches(&doc.symbols, &symbol_filters, &query_tokens) {
                if !matched_symbols.iter().any(|existing| existing == &item) {
                    matched_symbols.push(item);
                }
            }
            let matched_tokens = query_tokens
                .iter()
                .filter(|token| doc.terms.contains_key(*token))
                .cloned()
                .collect::<Vec<_>>();

            let mut score = base_score;
            if !matched_paths.is_empty() {
                score += 2.0;
            }
            if !matched_symbols.is_empty() {
                score += 1.5;
            }
            if !path_filters.is_empty() && matched_paths.is_empty() {
                continue;
            }
            if !symbol_filters.is_empty() && matched_symbols.is_empty() {
                continue;
            }
            if let Some(task_id) = task_filter.as_ref() {
                if doc.task_ids.iter().any(|item| item == task_id) {
                    score += match options.scope {
                        RecallScope::Global => 0.35,
                        RecallScope::TaskFirst | RecallScope::TaskOnly => 1.0,
                    };
                }
            }
            score += (1.0 / (1.0 + (age_secs as f64 / 86_400.0))).min(1.0) * 0.25;

            if score <= 0.0
                || (query_tokens.is_empty()
                    && matched_paths.is_empty()
                    && matched_symbols.is_empty())
            {
                continue;
            }

            let mut match_reasons = Vec::new();
            if !matched_tokens.is_empty() {
                match_reasons.push("bm25_text".to_string());
            }
            if !matched_paths.is_empty() {
                if basename_matches.is_empty() {
                    match_reasons.push("canonical_path_match".to_string());
                } else {
                    match_reasons.push("basename_fallback".to_string());
                }
            }
            if !matched_symbols.is_empty() {
                match_reasons.push("symbol_match".to_string());
            }
            if graph_overlap_candidates.contains(&cache_key)
                && (matched_tokens.is_empty()
                    || !match_reasons.iter().any(|reason| reason == "bm25_text"))
            {
                match_reasons.push("graph_overlap".to_string());
            }
            if let Some(task_id) = task_filter.as_ref() {
                if doc.task_ids.iter().any(|item| item == task_id) {
                    match_reasons.push("task_scope".to_string());
                }
            }

            hits.push(RecallHit {
                cache_key: doc.cache_key.clone(),
                target: doc.target.clone(),
                created_at_unix: doc.created_at_unix,
                age_secs,
                score,
                summary: doc.summary.clone(),
                snippet: doc.snippet.clone(),
                matched_tokens,
                matched_paths,
                matched_symbols,
                match_reasons,
                packet_types: doc.packet_types.clone(),
                task_ids: doc.task_ids.clone(),
                budget_estimate: doc.budget_estimate.clone(),
            });
        }

        hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| b.created_at_unix.cmp(&a.created_at_unix))
                .then_with(|| a.cache_key.cmp(&b.cache_key))
        });
        hits.into_iter().take(options.limit.max(1)).collect()
    }

    pub fn persist_file_path(root: &Path) -> PathBuf {
        persist_cache_path_v2(root)
    }

    fn collect_live_entries(&self, ttl_secs: u64) -> Vec<PersistPacketCacheEntry> {
        let now = now_unix();
        self.entries_by_hash
            .values()
            .filter(|entry| !is_expired(entry.created_at_unix, ttl_secs, now))
            .map(PersistPacketCacheEntry::from_entry)
            .collect()
    }

    fn remove_where<F>(&mut self, mut predicate: F, reason: EvictionReason) -> usize
    where
        F: FnMut(&PacketCacheEntry, u64) -> bool,
    {
        let now = now_unix();
        let before = self.entries_by_hash.len();
        let to_remove = self
            .entries_by_hash
            .iter()
            .filter(|(_, entry)| predicate(entry, now))
            .map(|(cache_key, _)| cache_key.clone())
            .collect::<Vec<_>>();
        for cache_key in &to_remove {
            self.entries_by_hash.remove(cache_key);
            self.remove_index_for(cache_key);
        }
        self.rebuild_latest_request_index();
        let removed = before.saturating_sub(self.entries_by_hash.len());
        if removed > 0 {
            self.evict_reason(reason, removed);
        }
        removed
    }

    fn evict_reason(&mut self, reason: EvictionReason, count: usize) {
        self.eviction_counters.add(reason, count);
    }

    fn rebuild_latest_request_index(&mut self) {
        let mut latest = HashMap::<String, (u64, String)>::new();

        for (cache_key, entry) in &self.entries_by_hash {
            let request_hash = Self::compute_request_hash(&entry.target, &entry.input_hash);
            let keep_newer = latest
                .get(&request_hash)
                .map(|(created, _)| entry.created_at_unix >= *created)
                .unwrap_or(true);
            if keep_newer {
                latest.insert(request_hash, (entry.created_at_unix, cache_key.clone()));
            }
        }

        self.latest_request_index = latest
            .into_iter()
            .map(|(request_hash, (_, cache_key))| (request_hash, cache_key))
            .collect();
    }

    fn rebuild_indexes(&mut self) {
        self.clear_indexes();
        let mut entries = self.entries_by_hash.values().cloned().collect::<Vec<_>>();
        entries.sort_by(|a, b| a.cache_key.cmp(&b.cache_key));
        for entry in &entries {
            self.index_entry(entry);
        }
    }

    fn clear_indexes(&mut self) {
        self.recall_docs.clear();
        self.recall_postings.clear();
        self.recall_avg_doc_length = 0.0;
        self.recall_total_doc_length = 0;
        self.file_ref_index.clear();
        self.basename_alias_index.clear();
        self.symbol_index.clear();
        self.test_index.clear();
        self.task_index.clear();
    }

    fn index_entry(&mut self, entry: &PacketCacheEntry) {
        let doc = build_recall_document(entry, self.workspace_root.as_deref());
        self.recall_total_doc_length = self.recall_total_doc_length.saturating_add(doc.doc_length);
        self.recall_avg_doc_length = if self.entries_by_hash.is_empty() {
            0.0
        } else {
            self.recall_total_doc_length as f64 / self.entries_by_hash.len() as f64
        };
        for (term, tf) in &doc.terms {
            self.recall_postings
                .entry(term.clone())
                .or_default()
                .push((doc.cache_key.clone(), *tf));
        }
        for path in &doc.paths {
            self.file_ref_index
                .entry(path.clone())
                .or_default()
                .insert(doc.cache_key.clone());
        }
        for basename in &doc.path_basenames {
            self.basename_alias_index
                .entry(basename.clone())
                .or_default()
                .extend(doc.paths.iter().cloned());
        }
        for symbol in &doc.symbols {
            self.symbol_index
                .entry(symbol.clone())
                .or_default()
                .insert(doc.cache_key.clone());
        }
        for test in &doc.tests {
            self.test_index
                .entry(test.clone())
                .or_default()
                .insert(doc.cache_key.clone());
        }
        for task_id in &doc.task_ids {
            self.task_index
                .entry(task_id.clone())
                .or_default()
                .insert(doc.cache_key.clone());
        }
        self.recall_docs.insert(doc.cache_key.clone(), doc);
    }

    fn remove_index_for(&mut self, cache_key: &str) {
        let Some(doc) = self.recall_docs.remove(cache_key) else {
            return;
        };
        self.recall_total_doc_length = self.recall_total_doc_length.saturating_sub(doc.doc_length);
        for term in doc.terms.keys() {
            if let Some(postings) = self.recall_postings.get_mut(term) {
                postings.retain(|(key, _)| key != cache_key);
                if postings.is_empty() {
                    self.recall_postings.remove(term);
                }
            }
        }
        remove_key_from_ref_index(&mut self.file_ref_index, &doc.paths, cache_key);
        for basename in &doc.path_basenames {
            if let Some(canonicals) = self.basename_alias_index.get_mut(basename) {
                for path in &doc.paths {
                    canonicals.remove(path);
                }
                if canonicals.is_empty() {
                    self.basename_alias_index.remove(basename);
                }
            }
        }
        remove_key_from_ref_index(&mut self.symbol_index, &doc.symbols, cache_key);
        remove_key_from_ref_index(&mut self.test_index, &doc.tests, cache_key);
        remove_key_from_ref_index(&mut self.task_index, &doc.task_ids, cache_key);
        self.recall_avg_doc_length = if self.recall_docs.is_empty() {
            0.0
        } else {
            self.recall_total_doc_length as f64 / self.recall_docs.len() as f64
        };
    }

    fn try_load_v2(&mut self, config: &PersistConfig) -> Option<()> {
        let raw = fs::read(persist_cache_path_v2(&config.root_dir)).ok()?;
        let envelope = match bincode::deserialize::<PersistEnvelopeV2>(&raw) {
            Ok(envelope) => envelope,
            Err(_) => {
                self.evict_reason(EvictionReason::CorruptLoadRecovery, 1);
                return None;
            }
        };
        if envelope.version != PERSIST_CACHE_VERSION {
            self.evict_reason(EvictionReason::VersionMismatch, 1);
            return None;
        }
        self.entries_by_hash.clear();
        for entry in envelope.entries {
            let entry = entry.into_entry();
            if !entry.cache_key.trim().is_empty() {
                self.entries_by_hash.insert(entry.cache_key.clone(), entry);
            }
        }
        self.recall_docs = envelope
            .recall_docs
            .into_iter()
            .map(|doc| (doc.cache_key.clone(), doc))
            .collect();
        self.recall_postings = envelope.recall_postings;
        self.recall_avg_doc_length = envelope.recall_avg_doc_length;
        self.recall_total_doc_length = self.recall_docs.values().map(|doc| doc.doc_length).sum();
        self.file_ref_index = envelope.file_ref_index;
        self.basename_alias_index = envelope.basename_alias_index;
        self.symbol_index = envelope.symbol_index;
        self.test_index = envelope.test_index;
        self.task_index = envelope.task_index;
        Some(())
    }

    fn try_load_v1(&mut self, config: &PersistConfig) -> Option<()> {
        let raw = fs::read(persist_cache_path_v1(&config.root_dir)).ok()?;
        let envelope = match bincode::deserialize::<PersistEnvelopeV1>(&raw) {
            Ok(envelope) => envelope,
            Err(_) => {
                self.evict_reason(EvictionReason::CorruptLoadRecovery, 1);
                return None;
            }
        };
        if envelope.version != 1 {
            self.evict_reason(EvictionReason::VersionMismatch, 1);
            return None;
        }
        self.entries_by_hash.clear();
        for entry in envelope.entries {
            let entry = entry.into_entry();
            if !entry.cache_key.trim().is_empty() {
                self.entries_by_hash.insert(entry.cache_key.clone(), entry);
            }
        }
        self.rebuild_indexes();
        Some(())
    }
}

fn persist_cache_path_v1(root: &Path) -> PathBuf {
    root.join(PERSIST_CACHE_DIR).join(PERSIST_CACHE_FILE_V1)
}

fn persist_cache_path_v2(root: &Path) -> PathBuf {
    root.join(PERSIST_CACHE_DIR).join(PERSIST_CACHE_FILE_V2)
}

fn tokenize(input: &str) -> Vec<String> {
    input
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != ':' && c != '/')
        .map(|part| part.trim().to_ascii_lowercase())
        .filter(|part| part.len() >= 2)
        .collect()
}

pub fn normalize_context_path(
    raw: &str,
    workspace_root: Option<&Path>,
) -> Option<NormalizedPathRef> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut normalized = trimmed.replace('\\', "/");
    if let Some(root) = workspace_root {
        let root = normalize_path_string(&root.to_string_lossy().replace('\\', "/"));
        let root = root.trim_matches('/').to_string();
        let absolute = normalize_path_string(&normalized);
        let absolute_trimmed = absolute.trim_matches('/').to_string();
        if !root.is_empty() && absolute_trimmed == root {
            normalized = ".".to_string();
        } else if !root.is_empty() && absolute_trimmed.starts_with(&(root.clone() + "/")) {
            normalized = absolute_trimmed[root.len() + 1..].to_string();
        } else {
            normalized = absolute;
        }
    } else {
        normalized = normalize_path_string(&normalized);
    }

    let canonical = normalized.trim_matches('/').to_ascii_lowercase();
    if canonical.is_empty() || canonical == "." {
        return None;
    }
    let basename = canonical
        .rsplit('/')
        .next()
        .map(str::trim)
        .filter(|basename| !basename.is_empty())
        .map(ToOwned::to_owned);
    Some(NormalizedPathRef {
        canonical,
        basename,
    })
}

pub fn basename_alias(raw: &str) -> Option<String> {
    normalize_context_path(raw, None).and_then(|path| path.basename)
}

fn normalize_path_string(raw: &str) -> String {
    let mut parts = Vec::<String>::new();
    let is_absolute = raw.starts_with('/');
    let raw_path = Path::new(raw);
    for component in raw_path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !parts.is_empty() && parts.last().is_some_and(|part| part != "..") {
                    parts.pop();
                } else if !is_absolute {
                    parts.push("..".to_string());
                }
            }
            Component::Normal(part) => parts.push(part.to_string_lossy().replace('\\', "/")),
            Component::RootDir => {}
            Component::Prefix(prefix) => {
                parts.push(prefix.as_os_str().to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let joined = parts.join("/");
    if is_absolute && !joined.is_empty() {
        format!("/{joined}")
    } else {
        joined
    }
}

fn extract_query_path_terms(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(|part| {
            part.trim_matches(|c: char| matches!(c, ',' | ';' | '"' | '\'' | '(' | ')' | '[' | ']'))
        })
        .filter(|part| looks_like_path(part))
        .map(ToOwned::to_owned)
        .collect()
}

fn push_normalized_path(
    paths: &mut Vec<String>,
    basenames: &mut Vec<String>,
    raw: &str,
    workspace_root: Option<&Path>,
) {
    let Some(path_ref) = normalize_context_path(raw, workspace_root) else {
        return;
    };
    push_unique_text(paths, &path_ref.canonical, usize::MAX);
    if let Some(basename) = path_ref.basename {
        push_unique_text(basenames, &basename, usize::MAX);
    }
}

fn filter_postings_for_live_keys(
    postings: &HashMap<String, Vec<(String, usize)>>,
    live_keys: &BTreeSet<String>,
) -> HashMap<String, Vec<(String, usize)>> {
    postings
        .iter()
        .filter_map(|(term, values)| {
            let filtered = values
                .iter()
                .filter(|(cache_key, _)| live_keys.contains(cache_key))
                .cloned()
                .collect::<Vec<_>>();
            (!filtered.is_empty()).then(|| (term.clone(), filtered))
        })
        .collect()
}

fn filter_ref_index_for_live_keys(
    index: &HashMap<String, BTreeSet<String>>,
    live_keys: &BTreeSet<String>,
) -> HashMap<String, BTreeSet<String>> {
    index
        .iter()
        .filter_map(|(term, values)| {
            let filtered = values
                .iter()
                .filter(|cache_key| live_keys.contains(*cache_key))
                .cloned()
                .collect::<BTreeSet<_>>();
            (!filtered.is_empty()).then(|| (term.clone(), filtered))
        })
        .collect()
}

fn remove_key_from_ref_index(
    index: &mut HashMap<String, BTreeSet<String>>,
    values: &[String],
    cache_key: &str,
) {
    let keys = values.to_vec();
    for value in keys {
        if let Some(cache_keys) = index.get_mut(&value) {
            cache_keys.remove(cache_key);
            if cache_keys.is_empty() {
                index.remove(&value);
            }
        }
    }
}

fn task_match_allowed(cache_key: &str, task_keys: Option<&BTreeSet<String>>) -> bool {
    task_keys
        .map(|keys| keys.contains(cache_key))
        .unwrap_or(true)
}

fn bm25_idf(doc_count: usize, posting_count: usize) -> f64 {
    (((doc_count.saturating_sub(posting_count) as f64) + 0.5) / (posting_count as f64 + 0.5) + 1.0)
        .ln()
}

fn bm25_score(tf: f64, doc_length: f64, avg_doc_length: f64, idf: f64) -> f64 {
    let k1 = 1.5;
    let b = 0.75;
    let norm = 1.0 - b + b * (doc_length / avg_doc_length.max(1.0));
    idf * (tf * (k1 + 1.0)) / (tf + k1 * norm)
}

fn collect_matches(
    haystack: &[String],
    explicit_filters: &[String],
    query_tokens: &[String],
) -> Vec<String> {
    let mut matches = Vec::new();
    let mut needles = explicit_filters.to_vec();
    for token in query_tokens {
        if !needles.iter().any(|existing| existing == token) {
            needles.push(token.clone());
        }
    }

    for item in haystack {
        let lower = item.to_ascii_lowercase();
        if needles
            .iter()
            .any(|needle| lower == *needle || lower.starts_with(needle) || lower.contains(needle))
            && !matches.iter().any(|existing| existing == item)
        {
            matches.push(item.clone());
        }
    }
    matches
}

fn build_recall_document(
    entry: &PacketCacheEntry,
    workspace_root: Option<&Path>,
) -> RecallDocument {
    let mut corpus = Vec::new();
    let mut summaries = Vec::new();
    let mut path_terms = Vec::new();
    let mut path_basenames = Vec::new();
    let mut symbol_terms = Vec::new();
    let mut test_terms = Vec::new();
    let mut packet_types = BTreeSet::new();
    let mut task_ids = BTreeSet::new();
    let mut budget_estimate = RecallBudgetEstimate::default();

    for packet in &entry.packets {
        collect_summary_texts(&packet.body, &mut summaries, 24);
        collect_summary_texts(&packet.metadata, &mut summaries, 24);
        if let Some(summary) = extract_packet_summary(&packet.body) {
            push_unique_text(&mut summaries, &summary, 24);
        }
        collect_texts_from_value(&packet.body, &mut corpus, 160);
        collect_texts_from_value(&packet.metadata, &mut corpus, 96);
        collect_ref_terms(
            &packet.body,
            workspace_root,
            &mut path_terms,
            &mut path_basenames,
            &mut symbol_terms,
            &mut test_terms,
        );
        collect_ref_terms(
            &packet.metadata,
            workspace_root,
            &mut path_terms,
            &mut path_basenames,
            &mut symbol_terms,
            &mut test_terms,
        );
        collect_task_ids(&packet.body, &mut task_ids);
        collect_task_ids(&packet.metadata, &mut task_ids);
        if let Some(packet_type) = packet
            .body
            .get("packet_type")
            .and_then(Value::as_str)
            .or_else(|| packet.body.get("kind").and_then(Value::as_str))
        {
            packet_types.insert(packet_type.to_ascii_lowercase());
        }
        let packet_budget =
            extract_budget_estimate(&packet.body, packet.token_usage, packet.runtime_ms);
        budget_estimate.est_tokens = budget_estimate
            .est_tokens
            .saturating_add(packet_budget.est_tokens);
        budget_estimate.est_bytes = budget_estimate
            .est_bytes
            .saturating_add(packet_budget.est_bytes);
        budget_estimate.runtime_ms = budget_estimate
            .runtime_ms
            .saturating_add(packet_budget.runtime_ms);
    }

    collect_summary_texts(&entry.metadata, &mut summaries, 24);
    collect_texts_from_value(&entry.metadata, &mut corpus, 96);
    collect_task_ids(&entry.metadata, &mut task_ids);
    corpus.extend(summaries.iter().cloned());
    corpus.push(entry.target.clone());
    corpus.push(entry.cache_key.clone());
    corpus.push(entry.input_hash.clone());
    corpus.extend(path_terms.iter().cloned());
    corpus.extend(path_basenames.iter().cloned());
    corpus.extend(symbol_terms.iter().cloned());
    corpus.extend(test_terms.iter().cloned());

    let summary = select_recall_summary(&summaries)
        .or_else(|| {
            corpus
                .iter()
                .filter(|item| !item.trim().is_empty())
                .max_by_key(|item| recall_summary_priority(item))
                .cloned()
        })
        .map(|item| truncate_recall_text(item, 200));
    let snippet = summary
        .clone()
        .or_else(|| corpus.iter().find(|item| !item.trim().is_empty()).cloned())
        .map(|item| truncate_recall_text(item, 200))
        .unwrap_or_else(|| "{}".to_string());

    let mut terms = HashMap::<String, usize>::new();
    let mut doc_length = 0_usize;
    for item in &corpus {
        for term in tokenize(item) {
            *terms.entry(term).or_insert(0) += 1;
            doc_length = doc_length.saturating_add(1);
        }
    }

    RecallDocument {
        cache_key: entry.cache_key.clone(),
        target: entry.target.clone(),
        created_at_unix: entry.created_at_unix,
        summary,
        snippet,
        task_ids: task_ids
            .into_iter()
            .map(|item| item.to_ascii_lowercase())
            .collect(),
        packet_types: packet_types.into_iter().collect(),
        paths: path_terms,
        path_basenames,
        symbols: symbol_terms,
        tests: test_terms,
        terms,
        doc_length,
        budget_estimate,
    }
}

fn extract_budget_estimate(
    body: &Value,
    token_usage: Option<u64>,
    runtime_ms: Option<u64>,
) -> RecallBudgetEstimate {
    let budget = body.get("budget_cost").and_then(Value::as_object);
    RecallBudgetEstimate {
        est_tokens: budget
            .and_then(|value| value.get("est_tokens"))
            .and_then(Value::as_u64)
            .or(token_usage)
            .unwrap_or_default(),
        est_bytes: budget
            .and_then(|value| value.get("est_bytes"))
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        runtime_ms: budget
            .and_then(|value| value.get("runtime_ms"))
            .and_then(Value::as_u64)
            .or(runtime_ms)
            .unwrap_or_default(),
    }
}

fn select_recall_summary(candidates: &[String]) -> Option<String> {
    candidates
        .iter()
        .filter(|item| !item.trim().is_empty())
        .max_by_key(|item| recall_summary_priority(item))
        .cloned()
}

fn recall_summary_priority(text: &str) -> i32 {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return i32::MIN;
    }

    let mut score = 0_i32;
    if !looks_like_path(trimmed) {
        score += 40;
    } else {
        score -= 40;
    }

    let word_count = trimmed.split_whitespace().count() as i32;
    score += word_count.min(12) * 2;
    score += (trimmed.len().min(180) as i32) / 12;

    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("context assemble ") || lower.starts_with("assembled context ") {
        score -= 60;
    }
    if lower.starts_with("repo map files=")
        || lower.starts_with("diff touched files=")
        || lower.starts_with("test impact selected_tests=")
        || lower.starts_with("build diagnostics unique=")
        || lower.starts_with("correlation findings=")
        || lower.starts_with("agent snapshot events=")
    {
        score -= 24;
    }
    if lower.contains("truncated=") && lower.contains("sections=") && lower.contains("refs=") {
        score -= 24;
    }

    score
}

fn extract_packet_summary(body: &Value) -> Option<String> {
    let tool = body.get("tool").and_then(Value::as_str).unwrap_or_default();
    let kind = body.get("kind").and_then(Value::as_str).unwrap_or_default();
    let payload = body.get("payload").unwrap_or(body);
    match (tool, kind) {
        ("stacky", "stack_slice") => Some(format!(
            "stack failures={} unique={}",
            payload
                .get("total_failures")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            payload
                .get("unique_failures")
                .and_then(Value::as_u64)
                .unwrap_or_default()
        )),
        ("mapy", "repo_map") => Some(format!(
            "repo map files={} symbols={}",
            payload
                .get("files_ranked")
                .and_then(Value::as_array)
                .map(|items| items.len())
                .unwrap_or_default(),
            payload
                .get("symbols_ranked")
                .and_then(Value::as_array)
                .map(|items| items.len())
                .unwrap_or_default()
        )),
        ("diffy", "diff_analyze") => Some(format!(
            "diff touched files={}",
            payload
                .get("diffs")
                .and_then(Value::as_array)
                .map(|items| items.len())
                .unwrap_or_default()
        )),
        ("testy", "test_impact") => Some(format!(
            "test impact selected_tests={}",
            payload
                .get("result")
                .and_then(|value| value.get("selected_tests"))
                .and_then(Value::as_array)
                .map(|items| items.len())
                .unwrap_or_default()
        )),
        ("buildy", "build_reduce") => Some(format!(
            "build diagnostics unique={}",
            payload
                .get("unique_diagnostics")
                .and_then(Value::as_u64)
                .unwrap_or_default()
        )),
        ("contextq", "context_correlate") => Some(format!(
            "correlation findings={}",
            payload
                .get("finding_count")
                .and_then(Value::as_u64)
                .unwrap_or_default()
        )),
        ("agenty", "agent_snapshot") => Some(format!(
            "agent snapshot events={} questions={}",
            payload
                .get("event_count")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            payload
                .get("open_questions")
                .and_then(Value::as_array)
                .map(|items| items.len())
                .unwrap_or_default()
        )),
        ("contextq", "context_assemble") => Some(format!(
            "assembled context sections={} refs={} truncated={}",
            payload
                .get("sections")
                .and_then(Value::as_array)
                .map(|items| items.len())
                .unwrap_or_default(),
            payload
                .get("refs")
                .and_then(Value::as_array)
                .map(|items| items.len())
                .unwrap_or_default(),
            payload
                .get("truncated")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        )),
        _ => None,
    }
}

fn collect_task_ids(value: &Value, out: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            if let Some(task_id) = map.get("task_id").and_then(Value::as_str) {
                let task_id = task_id.trim();
                if !task_id.is_empty() {
                    out.insert(task_id.to_string());
                }
            }
            for child in map.values() {
                collect_task_ids(child, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_task_ids(item, out);
            }
        }
        _ => {}
    }
}

fn looks_like_path(text: &str) -> bool {
    let trimmed = text.trim();
    !trimmed.is_empty()
        && (trimmed.contains('/')
            || trimmed.contains('\\')
            || trimmed.ends_with(".rs")
            || trimmed.ends_with(".java")
            || trimmed.ends_with(".kt")
            || trimmed.ends_with(".ts"))
}

fn collect_summary_texts(value: &Value, out: &mut Vec<String>, max_items: usize) {
    if out.len() >= max_items {
        return;
    }

    match value {
        Value::Object(map) => {
            if let Some(summary) = map.get("summary").and_then(Value::as_str) {
                push_unique_text(out, summary, max_items);
            }
            if (map.contains_key("title") || map.contains_key("source_packet"))
                && map
                    .get("body")
                    .and_then(Value::as_str)
                    .is_some_and(|body| !body.trim().is_empty())
            {
                push_unique_text(
                    out,
                    map.get("body").and_then(Value::as_str).unwrap_or_default(),
                    max_items,
                );
            }
            for child in map.values() {
                collect_summary_texts(child, out, max_items);
                if out.len() >= max_items {
                    return;
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_summary_texts(item, out, max_items);
                if out.len() >= max_items {
                    return;
                }
            }
        }
        _ => {}
    }
}

fn push_unique_text(out: &mut Vec<String>, text: &str, max_items: usize) {
    if out.len() >= max_items {
        return;
    }

    let trimmed = text.trim();
    if trimmed.is_empty() || out.iter().any(|existing| existing == trimmed) {
        return;
    }
    out.push(trimmed.to_string());
}

fn truncate_recall_text(mut text: String, max_len: usize) -> String {
    if text.len() > max_len {
        text.truncate(max_len);
    }
    text
}

fn collect_texts_from_value(value: &Value, out: &mut Vec<String>, max_items: usize) {
    if out.len() >= max_items {
        return;
    }
    match value {
        Value::String(text) => {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                out.push(trimmed.to_string());
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_texts_from_value(item, out, max_items);
                if out.len() >= max_items {
                    return;
                }
            }
        }
        Value::Object(map) => {
            for value in map.values() {
                collect_texts_from_value(value, out, max_items);
                if out.len() >= max_items {
                    return;
                }
            }
        }
        _ => {}
    }
}

fn collect_ref_terms(
    value: &Value,
    workspace_root: Option<&Path>,
    paths: &mut Vec<String>,
    path_basenames: &mut Vec<String>,
    symbols: &mut Vec<String>,
    tests: &mut Vec<String>,
) {
    match value {
        Value::Object(map) => {
            if let Some(path) = map.get("path").and_then(Value::as_str) {
                push_normalized_path(paths, path_basenames, path, workspace_root);
            }
            if let Some(file) = map.get("file").and_then(Value::as_str) {
                push_normalized_path(paths, path_basenames, file, workspace_root);
            }
            if let Some(name) = map.get("name").and_then(Value::as_str) {
                push_unique_text(symbols, &name.to_ascii_lowercase(), usize::MAX);
            }
            if let Some(test_id) = map.get("test_id").and_then(Value::as_str) {
                push_unique_text(tests, &test_id.to_ascii_lowercase(), usize::MAX);
            }
            if map
                .get("kind")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind.eq_ignore_ascii_case("file"))
            {
                if let Some(value) = map.get("value").and_then(Value::as_str) {
                    push_normalized_path(paths, path_basenames, value, workspace_root);
                }
            }
            if map
                .get("kind")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind.eq_ignore_ascii_case("symbol"))
            {
                if let Some(value) = map.get("value").and_then(Value::as_str) {
                    push_unique_text(symbols, &value.to_ascii_lowercase(), usize::MAX);
                }
            }
            if let Some(selected_tests) = map.get("selected_tests").and_then(Value::as_array) {
                for test in selected_tests {
                    if let Some(test) = test.as_str() {
                        push_unique_text(tests, &test.to_ascii_lowercase(), usize::MAX);
                    }
                }
            }
            for child in map.values() {
                collect_ref_terms(child, workspace_root, paths, path_basenames, symbols, tests);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_ref_terms(item, workspace_root, paths, path_basenames, symbols, tests);
            }
        }
        Value::String(text) => {
            if looks_like_path(text) {
                push_normalized_path(paths, path_basenames, text, workspace_root);
            } else if text.contains("::") {
                push_unique_text(symbols, &text.to_ascii_lowercase(), usize::MAX);
            }
        }
        _ => {}
    }
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), io::Error> {
    let temp_path = path.with_extension("tmp");
    fs::write(&temp_path, bytes)?;

    match fs::rename(&temp_path, path) {
        Ok(()) => Ok(()),
        Err(_) => {
            fs::write(path, bytes)?;
            let _ = fs::remove_file(&temp_path);
            Ok(())
        }
    }
}

fn is_expired(created_at_unix: u64, ttl_secs: u64, now_unix: u64) -> bool {
    if ttl_secs == 0 {
        return false;
    }
    now_unix.saturating_sub(created_at_unix) > ttl_secs
}

fn encode_json_value(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

fn decode_json_value(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or(Value::Null)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;
    use tempfile::tempdir;

    #[test]
    fn stores_and_reads_by_hash() {
        let mut cache = PacketCache::new();
        let mut hooks = NoopDeltaReuseHooks;
        let reducer_input = serde_json::json!({"k":"v"});
        let lookup = cache.lookup_with_hooks("demo.reducer", &reducer_input, &mut hooks);

        let stored = cache.put_with_hooks(
            "demo.reducer",
            &lookup,
            vec![CachePacket {
                packet_id: Some("one".to_string()),
                body: serde_json::json!({"ok":true}),
                ..CachePacket::default()
            }],
            serde_json::json!({"cached": true}),
            &mut hooks,
        );

        let from_hash = cache.get(&stored.cache_key).unwrap();
        assert_eq!(from_hash.target, "demo.reducer");
        assert_eq!(from_hash.packets.len(), 1);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn lookup_hits_after_put() {
        let mut cache = PacketCache::new();
        let mut hooks = NoopDeltaReuseHooks;
        let reducer_input = serde_json::json!({"task":"a"});

        let lookup = cache.lookup_with_hooks("demo.reducer", &reducer_input, &mut hooks);
        assert!(lookup.entry.is_none());

        cache.put_with_hooks(
            "demo.reducer",
            &lookup,
            vec![CachePacket {
                body: serde_json::json!({"n":1}),
                ..CachePacket::default()
            }],
            Value::Null,
            &mut hooks,
        );

        let second = cache.lookup_with_hooks("demo.reducer", &reducer_input, &mut hooks);
        assert!(second.entry.is_some());
    }

    struct CapturingHooks {
        hits: Rc<RefCell<Vec<String>>>,
        puts: Rc<RefCell<Vec<String>>>,
    }

    impl DeltaReuseHooks for CapturingHooks {
        fn on_hit(&mut self, entry: &PacketCacheEntry) {
            self.hits.borrow_mut().push(entry.cache_key.clone());
        }

        fn on_put(&mut self, entry: &PacketCacheEntry) {
            self.puts.borrow_mut().push(entry.cache_key.clone());
        }
    }

    #[test]
    fn hooks_receive_hit_and_put_events() {
        let hits = Rc::new(RefCell::new(Vec::new()));
        let puts = Rc::new(RefCell::new(Vec::new()));
        let mut hooks = CapturingHooks {
            hits: hits.clone(),
            puts: puts.clone(),
        };

        let mut cache = PacketCache::new();
        let reducer_input = serde_json::json!({"task":"b"});
        let lookup = cache.lookup_with_hooks("demo.reducer", &reducer_input, &mut hooks);

        cache.put_with_hooks(
            "demo.reducer",
            &lookup,
            vec![CachePacket::default()],
            Value::Null,
            &mut hooks,
        );

        let _ = cache.lookup_with_hooks("demo.reducer", &reducer_input, &mut hooks);
        assert_eq!(puts.borrow().len(), 1);
        assert_eq!(hits.borrow().len(), 1);
    }

    #[test]
    fn stores_and_loads_from_disk_roundtrip() {
        let dir = tempdir().unwrap();
        let config = PersistConfig::new(dir.path().to_path_buf());

        let mut cache = PacketCache::new();
        let mut hooks = NoopDeltaReuseHooks;
        let reducer_input = serde_json::json!({"task":"persist"});
        let lookup = cache.lookup_with_hooks("demo.reducer", &reducer_input, &mut hooks);
        let request_hash = lookup.input_hash.clone();
        cache.put_with_hooks(
            "demo.reducer",
            &lookup,
            vec![CachePacket {
                body: serde_json::json!({"persisted": true}),
                ..CachePacket::default()
            }],
            Value::Null,
            &mut hooks,
        );

        cache.save_to_disk(&config).unwrap();
        let cache_path = persist_cache_path_v2(dir.path());
        let raw = fs::read(cache_path).unwrap();
        let envelope: PersistEnvelopeV2 = bincode::deserialize(&raw).unwrap();
        assert_eq!(envelope.version, PERSIST_CACHE_VERSION);
        assert_eq!(envelope.entries.len(), 1);
        assert!(!envelope.recall_docs.is_empty());

        let loaded = PacketCache::load_from_disk(&config);
        assert_eq!(loaded.len(), 1);
        assert!(loaded
            .get_by_request("demo.reducer", &request_hash)
            .is_some());
    }

    #[test]
    fn evicts_expired_entries() {
        let mut cache = PacketCache::new();
        let mut hooks = NoopDeltaReuseHooks;
        let reducer_input = serde_json::json!({"task":"ttl"});
        let lookup = cache.lookup_with_hooks("demo.reducer", &reducer_input, &mut hooks);
        let stored = cache.put_with_hooks(
            "demo.reducer",
            &lookup,
            vec![CachePacket::default()],
            Value::Null,
            &mut hooks,
        );

        let old = now_unix().saturating_sub(3_600);
        cache
            .entries_by_hash
            .get_mut(&stored.cache_key)
            .unwrap()
            .created_at_unix = old;

        cache.evict_expired(60);
        assert!(cache.is_empty());
    }

    #[test]
    fn load_from_disk_ignores_corrupt_file() {
        let dir = tempdir().unwrap();
        let config = PersistConfig::new(dir.path().to_path_buf());
        let path = persist_cache_path_v2(dir.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"this-is-not-bincode").unwrap();

        let loaded = PacketCache::load_from_disk(&config);
        assert!(loaded.is_empty());
        assert_eq!(loaded.stats().evictions.corrupt_load_recovery, 1);
    }

    #[test]
    fn load_from_v1_rebuilds_indexes_and_migrates_forward() {
        let dir = tempdir().unwrap();
        let config = PersistConfig::new(dir.path().to_path_buf());
        let mut cache = PacketCache::new();
        let mut hooks = NoopDeltaReuseHooks;
        let lookup = cache.lookup_with_hooks(
            "demo.reducer",
            &serde_json::json!({"task_id":"task-v1"}),
            &mut hooks,
        );
        cache.put_with_hooks(
            "demo.reducer",
            &lookup,
            vec![CachePacket {
                body: serde_json::json!({
                    "summary": "legacy cache for src/auth/StringUtils.java",
                    "task_id": "task-v1",
                    "files": [{"path": "src/auth/StringUtils.java"}],
                    "symbols": [{"name": "normalize"}],
                }),
                ..CachePacket::default()
            }],
            Value::Null,
            &mut hooks,
        );

        let legacy_envelope = PersistEnvelopeV1 {
            version: 1,
            entries: cache.collect_live_entries(config.ttl_secs),
        };
        let path = persist_cache_path_v1(dir.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bincode::serialize(&legacy_envelope).unwrap()).unwrap();

        let loaded = PacketCache::load_from_disk(&config);
        let hits = loaded.recall(
            "StringUtils.java",
            &RecallOptions {
                limit: 4,
                task_id: Some("task-v1".to_string()),
                scope: RecallScope::TaskOnly,
                ..RecallOptions::default()
            },
        );
        assert_eq!(loaded.len(), 1);
        assert!(!hits.is_empty());
        assert!(hits[0]
            .match_reasons
            .iter()
            .any(|reason| reason == "basename_fallback" || reason == "canonical_path_match"));
    }

    #[test]
    fn list_get_and_stats_surface_entry_details() {
        let mut cache = PacketCache::new();
        let mut hooks = NoopDeltaReuseHooks;
        let lookup =
            cache.lookup_with_hooks("demo.reducer", &serde_json::json!({"task":"x"}), &mut hooks);
        let stored = cache.put_with_hooks(
            "demo.reducer",
            &lookup,
            vec![CachePacket {
                body: serde_json::json!({
                    "paths": ["src/lib.rs"],
                    "refs": [{"kind":"symbol","value":"run"}],
                    "summary": "hello world"
                }),
                ..CachePacket::default()
            }],
            serde_json::json!({"source":"test"}),
            &mut hooks,
        );

        let listed = cache.list_entries(
            &ContextStoreListFilter::default(),
            &ContextStorePaging::default(),
        );
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].cache_key, stored.cache_key);

        let detail = cache.get_entry(&stored.cache_key).unwrap();
        assert_eq!(detail.entry.target, "demo.reducer");

        let stats = cache.stats();
        assert_eq!(stats.entries, 1);
        assert!(stats.oldest_created_at_unix.is_some());
    }

    #[test]
    fn prune_and_recall_produce_expected_outputs() {
        let mut cache = PacketCache::new();
        let mut hooks = NoopDeltaReuseHooks;
        let lookup = cache.lookup_with_hooks(
            "demo.reducer",
            &serde_json::json!({"task":"recall"}),
            &mut hooks,
        );
        let stored = cache.put_with_hooks(
            "demo.reducer",
            &lookup,
            vec![CachePacket {
                body: serde_json::json!({
                    "summary": "parser crash investigation for src/main.rs",
                    "sections":[{"body":"Investigated parser crash in src/main.rs"}],
                    "refs":[{"kind":"file","value":"src/main.rs"},{"kind":"symbol","value":"parse_input"}]
                }),
                ..CachePacket::default()
            }],
            Value::Null,
            &mut hooks,
        );

        let hits = cache.recall(
            "parser crash src/main.rs",
            &RecallOptions {
                limit: 3,
                ..RecallOptions::default()
            },
        );
        assert!(!hits.is_empty());
        assert_eq!(hits[0].cache_key, stored.cache_key);
        assert_eq!(
            hits[0].summary.as_deref(),
            Some("parser crash investigation for src/main.rs")
        );
        assert_eq!(
            hits[0].snippet,
            "parser crash investigation for src/main.rs"
        );

        let report = cache.prune(ContextStorePruneRequest {
            all: true,
            ttl_secs: None,
        });
        assert_eq!(report.removed, 1);
        assert_eq!(report.reasons.manual_prune, 1);
        assert!(cache.is_empty());
    }

    #[test]
    fn recall_respects_task_scope_and_structured_filters() {
        let mut cache = PacketCache::new();
        let mut hooks = NoopDeltaReuseHooks;

        for (task_id, path, symbol) in [
            ("task-a", "src/auth.rs", "authenticate"),
            ("task-b", "src/billing.rs", "invoice"),
        ] {
            let lookup = cache.lookup_with_hooks(
                "demo.reducer",
                &serde_json::json!({ "task_id": task_id }),
                &mut hooks,
            );
            cache.put_with_hooks(
                "demo.reducer",
                &lookup,
                vec![CachePacket {
                    body: serde_json::json!({
                        "tool": "contextq",
                        "kind": "context_manage",
                        "packet_type": "suite.context.manage.v1",
                        "task_id": task_id,
                        "summary": format!("investigation for {path}"),
                        "files": [{"path": path}],
                        "symbols": [{"name": symbol}],
                        "budget_cost": {"est_tokens": 32, "est_bytes": 128, "runtime_ms": 5}
                    }),
                    ..CachePacket::default()
                }],
                Value::Null,
                &mut hooks,
            );
        }

        let hits = cache.recall(
            "auth",
            &RecallOptions {
                task_id: Some("task-a".to_string()),
                scope: RecallScope::TaskOnly,
                packet_types: vec!["context.manage".to_string()],
                path_filters: vec!["auth".to_string()],
                symbol_filters: vec!["authenticate".to_string()],
                ..RecallOptions::default()
            },
        );

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].task_ids, vec!["task-a".to_string()]);
        assert!(hits[0]
            .matched_paths
            .iter()
            .any(|path| path.contains("auth")));
        assert!(hits[0]
            .match_reasons
            .iter()
            .any(|reason| reason == "task_scope"));
        assert_eq!(hits[0].budget_estimate.est_tokens, 32);
    }
}
