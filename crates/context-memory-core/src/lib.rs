use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const DEFAULT_PERSIST_TTL_SECS: u64 = 86_400;
const PERSIST_CACHE_VERSION: u32 = 1;
const PERSIST_CACHE_DIR: &str = ".packet28";
const PERSIST_CACHE_FILE: &str = "packet-cache-v1.bin";

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
}

impl Default for RecallOptions {
    fn default() -> Self {
        Self {
            limit: 8,
            since_unix: None,
            until_unix: None,
            target: None,
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
    pub snippet: String,
    pub matched_tokens: Vec<String>,
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
struct PersistEnvelope {
    version: u32,
    entries: Vec<PersistPacketCacheEntry>,
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
}

impl PacketCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn load_from_disk(config: &PersistConfig) -> Self {
        let mut cache = Self::new();
        let path = persist_cache_path(&config.root_dir);
        let Ok(raw) = fs::read(path) else {
            return cache;
        };

        let Ok(envelope) = bincode::deserialize::<PersistEnvelope>(&raw) else {
            cache
                .eviction_counters
                .add(EvictionReason::CorruptLoadRecovery, 1);
            return cache;
        };

        if envelope.version != PERSIST_CACHE_VERSION {
            cache
                .eviction_counters
                .add(EvictionReason::VersionMismatch, 1);
            return cache;
        }

        for entry in envelope.entries {
            let entry = entry.into_entry();
            if entry.cache_key.trim().is_empty() {
                continue;
            }
            cache.entries_by_hash.insert(entry.cache_key.clone(), entry);
        }

        cache.rebuild_latest_request_index();
        cache.evict_expired(config.ttl_secs);
        cache
    }

    pub fn save_to_disk(&self, config: &PersistConfig) -> Result<(), io::Error> {
        let path = persist_cache_path(&config.root_dir);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let envelope = PersistEnvelope {
            version: PERSIST_CACHE_VERSION,
            entries: self.collect_live_entries(config.ttl_secs),
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
        let now = now_unix();
        let before = self.entries_by_hash.len();
        self.entries_by_hash
            .retain(|_, entry| !is_expired(entry.created_at_unix, ttl_secs, now));
        self.rebuild_latest_request_index();
        let removed = before.saturating_sub(self.entries_by_hash.len());
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

        self.entries_by_hash
            .insert(entry.cache_key.clone(), entry.clone());
        self.latest_request_index
            .insert(lookup.cache_key.clone(), entry.cache_key.clone());
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

    pub fn prune(&mut self, request: ContextStorePruneRequest) -> ContextStorePruneReport {
        let removed = if request.all {
            let removed = self.entries_by_hash.len();
            self.entries_by_hash.clear();
            self.latest_request_index.clear();
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
        if query_tokens.is_empty() {
            return Vec::new();
        }

        let now = now_unix();
        let target_filter = options.target.as_ref().map(|v| v.to_ascii_lowercase());

        let mut hits = Vec::new();
        for entry in self.entries_by_hash.values() {
            if let Some(target) = target_filter.as_ref() {
                if !entry.target.to_ascii_lowercase().contains(target) {
                    continue;
                }
            }
            if let Some(since) = options.since_unix {
                if entry.created_at_unix < since {
                    continue;
                }
            }
            if let Some(until) = options.until_unix {
                if entry.created_at_unix > until {
                    continue;
                }
            }

            let age_secs = now.saturating_sub(entry.created_at_unix);
            let (score, snippet, matched_tokens) =
                score_recall_entry(entry, &query_tokens, age_secs);
            if score <= 0.0 {
                continue;
            }

            hits.push(RecallHit {
                cache_key: entry.cache_key.clone(),
                target: entry.target.clone(),
                created_at_unix: entry.created_at_unix,
                age_secs,
                score,
                snippet,
                matched_tokens,
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
        persist_cache_path(root)
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
        self.entries_by_hash
            .retain(|_, entry| !predicate(entry, now));
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
}

fn persist_cache_path(root: &Path) -> PathBuf {
    root.join(PERSIST_CACHE_DIR).join(PERSIST_CACHE_FILE)
}

fn tokenize(input: &str) -> Vec<String> {
    input
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != ':' && c != '/')
        .map(|part| part.trim().to_ascii_lowercase())
        .filter(|part| part.len() >= 2)
        .collect()
}

fn score_recall_entry(
    entry: &PacketCacheEntry,
    query_tokens: &[String],
    age_secs: u64,
) -> (f64, String, Vec<String>) {
    let mut corpus = Vec::new();
    corpus.push(entry.target.clone());
    corpus.push(entry.cache_key.clone());
    corpus.push(entry.input_hash.clone());
    collect_texts_from_value(&entry.metadata, &mut corpus, 64);

    let mut path_terms = Vec::new();
    let mut symbol_terms = Vec::new();
    for packet in &entry.packets {
        collect_texts_from_value(&packet.body, &mut corpus, 128);
        collect_texts_from_value(&packet.metadata, &mut corpus, 64);
        collect_ref_terms(&packet.body, &mut path_terms, &mut symbol_terms);
    }

    let corpus_lower = corpus
        .iter()
        .map(|item| item.to_ascii_lowercase())
        .collect::<Vec<_>>();

    let mut matched_tokens = Vec::new();
    for token in query_tokens {
        if corpus_lower.iter().any(|item| item.contains(token)) {
            matched_tokens.push(token.clone());
        }
    }

    if matched_tokens.is_empty() {
        return (0.0, String::new(), Vec::new());
    }

    let base = matched_tokens.len() as f64 / query_tokens.len() as f64;
    let path_boost = if query_tokens
        .iter()
        .any(|token| path_terms.iter().any(|path| path.contains(token)))
    {
        0.2
    } else {
        0.0
    };
    let symbol_boost = if query_tokens
        .iter()
        .any(|token| symbol_terms.iter().any(|symbol| symbol.contains(token)))
    {
        0.2
    } else {
        0.0
    };
    let recency_boost = (1.0 / (1.0 + (age_secs as f64 / 86_400.0))).min(1.0) * 0.2;

    let mut snippet = corpus
        .iter()
        .find(|item| {
            let lower = item.to_ascii_lowercase();
            matched_tokens.iter().any(|token| lower.contains(token))
        })
        .cloned()
        .unwrap_or_else(|| "{}".to_string());
    if snippet.len() > 200 {
        snippet.truncate(200);
    }

    (
        base + path_boost + symbol_boost + recency_boost,
        snippet,
        matched_tokens,
    )
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

fn collect_ref_terms(value: &Value, paths: &mut Vec<String>, symbols: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            if let Some(path) = map.get("path").and_then(Value::as_str) {
                paths.push(path.to_ascii_lowercase());
            }
            if let Some(file) = map.get("file").and_then(Value::as_str) {
                paths.push(file.to_ascii_lowercase());
            }
            if let Some(name) = map.get("name").and_then(Value::as_str) {
                symbols.push(name.to_ascii_lowercase());
            }
            if map
                .get("kind")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind.eq_ignore_ascii_case("file"))
            {
                if let Some(value) = map.get("value").and_then(Value::as_str) {
                    paths.push(value.to_ascii_lowercase());
                }
            }
            if map
                .get("kind")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind.eq_ignore_ascii_case("symbol"))
            {
                if let Some(value) = map.get("value").and_then(Value::as_str) {
                    symbols.push(value.to_ascii_lowercase());
                }
            }
            for child in map.values() {
                collect_ref_terms(child, paths, symbols);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_ref_terms(item, paths, symbols);
            }
        }
        Value::String(text) => {
            if text.contains('/') || text.contains('\\') || text.contains("::") {
                paths.push(text.to_ascii_lowercase());
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
        let cache_path = persist_cache_path(dir.path());
        let raw = fs::read(cache_path).unwrap();
        let envelope: PersistEnvelope = bincode::deserialize(&raw).unwrap();
        assert_eq!(envelope.version, PERSIST_CACHE_VERSION);
        assert_eq!(envelope.entries.len(), 1);

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
        let path = persist_cache_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"this-is-not-bincode").unwrap();

        let loaded = PacketCache::load_from_disk(&config);
        assert!(loaded.is_empty());
        assert_eq!(loaded.stats().evictions.corrupt_load_recovery, 1);
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

        let report = cache.prune(ContextStorePruneRequest {
            all: true,
            ttl_secs: None,
        });
        assert_eq!(report.removed, 1);
        assert_eq!(report.reasons.manual_prune, 1);
        assert!(cache.is_empty());
    }
}
