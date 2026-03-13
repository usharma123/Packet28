use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::*;

#[derive(Default)]
pub struct PacketCache {
    pub(crate) entries_by_hash: HashMap<String, PacketCacheEntry>,
    pub(crate) latest_request_index: HashMap<String, String>,
    pub(crate) eviction_counters: EvictionCounters,
    pub(crate) workspace_root: Option<PathBuf>,
    pub(crate) recall_docs: HashMap<String, RecallDocument>,
    pub(crate) recall_postings: HashMap<String, Vec<(String, usize)>>,
    pub(crate) recall_avg_doc_length: f64,
    pub(crate) recall_total_doc_length: usize,
    pub(crate) file_ref_index: HashMap<String, BTreeSet<String>>,
    pub(crate) basename_alias_index: HashMap<String, BTreeSet<String>>,
    pub(crate) symbol_index: HashMap<String, BTreeSet<String>>,
    pub(crate) test_index: HashMap<String, BTreeSet<String>>,
    pub(crate) task_index: HashMap<String, BTreeSet<String>>,
}

impl PacketCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn evict_expired(&mut self, ttl_secs: u64) {
        self.remove_where(
            |entry, now| is_expired(entry.created_at_unix, ttl_secs, now),
            EvictionReason::ExpiredTtl,
        );
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

    pub(crate) fn remove_where<F>(&mut self, mut predicate: F, reason: EvictionReason) -> usize
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

    pub(crate) fn evict_reason(&mut self, reason: EvictionReason, count: usize) {
        self.eviction_counters.add(reason, count);
    }

    pub(crate) fn rebuild_latest_request_index(&mut self) {
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

    pub(crate) fn rebuild_indexes(&mut self) {
        self.clear_indexes();
        let mut entries = self.entries_by_hash.values().cloned().collect::<Vec<_>>();
        entries.sort_by(|a, b| a.cache_key.cmp(&b.cache_key));
        for entry in &entries {
            self.index_entry(entry);
        }
    }

    pub(crate) fn clear_indexes(&mut self) {
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

    pub(crate) fn index_entry(&mut self, entry: &PacketCacheEntry) {
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
        for path in &doc.paths {
            if let Some(basename) = basename_alias(path) {
                self.basename_alias_index
                    .entry(basename)
                    .or_default()
                    .insert(path.clone());
            }
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

    pub(crate) fn remove_index_for(&mut self, cache_key: &str) {
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
        let orphaned_paths = doc
            .paths
            .iter()
            .filter(|path| !self.file_ref_index.contains_key(*path))
            .cloned()
            .collect::<Vec<_>>();
        for path in &orphaned_paths {
            if let Some(basename) = basename_alias(path) {
                if let Some(canonicals) = self.basename_alias_index.get_mut(&basename) {
                    canonicals.remove(path);
                    if canonicals.is_empty() {
                        self.basename_alias_index.remove(&basename);
                    }
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
}

pub(crate) fn remove_key_from_ref_index(
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

pub(crate) fn is_expired(created_at_unix: u64, ttl_secs: u64, now_unix: u64) -> bool {
    if ttl_secs == 0 {
        return false;
    }
    now_unix.saturating_sub(created_at_unix) > ttl_secs
}

pub(crate) fn encode_json_value(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

pub(crate) fn decode_json_value(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or(Value::Null)
}

pub(crate) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::fs;
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
        assert_eq!(cache.stats().evictions.expired_ttl, 1);
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

    #[test]
    fn basename_aliases_index_each_path_independently() {
        let mut cache = PacketCache::new();
        let mut hooks = NoopDeltaReuseHooks;
        let lookup =
            cache.lookup_with_hooks("demo.reducer", &serde_json::json!({"task":"paths"}), &mut hooks);
        cache.put_with_hooks(
            "demo.reducer",
            &lookup,
            vec![CachePacket {
                body: serde_json::json!({
                    "files": [
                        {"path": "src/alpha.rs"},
                        {"path": "src/beta.rs"}
                    ]
                }),
                ..CachePacket::default()
            }],
            Value::Null,
            &mut hooks,
        );

        assert_eq!(
            cache.basename_alias_index.get("alpha.rs"),
            Some(&BTreeSet::from(["src/alpha.rs".to_string()]))
        );
        assert_eq!(
            cache.basename_alias_index.get("beta.rs"),
            Some(&BTreeSet::from(["src/beta.rs".to_string()]))
        );
    }

    #[test]
    fn removing_one_entry_keeps_shared_basename_aliases_live() {
        let mut cache = PacketCache::new();
        let mut hooks = NoopDeltaReuseHooks;

        let first_lookup =
            cache.lookup_with_hooks("demo.reducer", &serde_json::json!({"task":"one"}), &mut hooks);
        let first = cache.put_with_hooks(
            "demo.reducer",
            &first_lookup,
            vec![CachePacket {
                body: serde_json::json!({"files": [{"path": "src/shared.rs"}]}),
                ..CachePacket::default()
            }],
            Value::Null,
            &mut hooks,
        );

        let second_lookup =
            cache.lookup_with_hooks("demo.reducer", &serde_json::json!({"task":"two"}), &mut hooks);
        cache.put_with_hooks(
            "demo.reducer",
            &second_lookup,
            vec![CachePacket {
                body: serde_json::json!({"files": [{"path": "src/shared.rs"}]}),
                ..CachePacket::default()
            }],
            Value::Null,
            &mut hooks,
        );

        cache.remove_where(|entry, _| entry.cache_key == first.cache_key, EvictionReason::ManualPrune);

        assert_eq!(
            cache.basename_alias_index.get("shared.rs"),
            Some(&BTreeSet::from(["src/shared.rs".to_string()]))
        );
    }

    #[test]
    fn tokenize_expands_identifier_fragments() {
        let tokens =
            tokenize("StringUtils.seededRandom src/main/java/org/apache/FastDateParser.java");

        assert!(tokens.iter().any(|token| token == "stringutils"));
        assert!(tokens.iter().any(|token| token == "string"));
        assert!(tokens.iter().any(|token| token == "utils"));
        assert!(tokens.iter().any(|token| token == "seededrandom"));
        assert!(tokens.iter().any(|token| token == "seeded"));
        assert!(tokens.iter().any(|token| token == "random"));
        assert!(tokens.iter().any(|token| token == "fastdateparser"));
        assert!(tokens.iter().any(|token| token == "fast"));
        assert!(tokens.iter().any(|token| token == "date"));
        assert!(tokens.iter().any(|token| token == "parser"));
    }
}
