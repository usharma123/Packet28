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
}

impl PacketCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn load_from_disk(config: &PersistConfig) -> Self {
        let path = persist_cache_path(&config.root_dir);
        let Ok(raw) = fs::read(path) else {
            return Self::new();
        };

        let Ok(envelope) = bincode::deserialize::<PersistEnvelope>(&raw) else {
            return Self::new();
        };

        if envelope.version != PERSIST_CACHE_VERSION {
            return Self::new();
        }

        let mut cache = Self::new();
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
        self.entries_by_hash
            .retain(|_, entry| !is_expired(entry.created_at_unix, ttl_secs, now));
        self.rebuild_latest_request_index();
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

    fn collect_live_entries(&self, ttl_secs: u64) -> Vec<PersistPacketCacheEntry> {
        let now = now_unix();
        self.entries_by_hash
            .values()
            .filter(|entry| !is_expired(entry.created_at_unix, ttl_secs, now))
            .map(PersistPacketCacheEntry::from_entry)
            .collect()
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
    }
}
