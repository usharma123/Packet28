use super::*;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct PersistEnvelopeV1 {
    pub(crate) version: u32,
    pub(crate) entries: Vec<PersistPacketCacheEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub(crate) struct PersistEnvelopeV2 {
    pub(crate) version: u32,
    pub(crate) entries: Vec<PersistPacketCacheEntry>,
    pub(crate) recall_docs: Vec<RecallDocument>,
    pub(crate) recall_postings: HashMap<String, Vec<(String, usize)>>,
    pub(crate) recall_avg_doc_length: f64,
    pub(crate) file_ref_index: HashMap<String, BTreeSet<String>>,
    pub(crate) basename_alias_index: HashMap<String, BTreeSet<String>>,
    pub(crate) symbol_index: HashMap<String, BTreeSet<String>>,
    pub(crate) test_index: HashMap<String, BTreeSet<String>>,
    pub(crate) task_index: HashMap<String, BTreeSet<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct PersistPacketCacheEntry {
    cache_key: String,
    target: String,
    input_hash: String,
    created_at_unix: u64,
    packets: Vec<PersistCachePacket>,
    metadata_json: String,
    delta_reuse: DeltaReuse,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct PersistCachePacket {
    packet_id: Option<String>,
    body_json: String,
    token_usage: Option<u64>,
    runtime_ms: Option<u64>,
    metadata_json: String,
}

impl PersistPacketCacheEntry {
    pub(crate) fn from_entry(entry: &PacketCacheEntry) -> Self {
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

    pub(crate) fn into_entry(self) -> PacketCacheEntry {
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
    pub(crate) fn from_cache_packet(packet: &CachePacket) -> Self {
        Self {
            packet_id: packet.packet_id.clone(),
            body_json: encode_json_value(&packet.body),
            token_usage: packet.token_usage,
            runtime_ms: packet.runtime_ms,
            metadata_json: encode_json_value(&packet.metadata),
        }
    }

    pub(crate) fn into_cache_packet(self) -> CachePacket {
        CachePacket {
            packet_id: self.packet_id,
            body: decode_json_value(&self.body_json),
            token_usage: self.token_usage,
            runtime_ms: self.runtime_ms,
            metadata: decode_json_value(&self.metadata_json),
        }
    }
}

impl PacketCache {
    pub fn load_from_disk(config: &PersistConfig) -> Self {
        let mut cache = Self {
            workspace_root: Some(config.root_dir.clone()),
            ..Self::new()
        };
        let mut v2_cache = Self {
            workspace_root: Some(config.root_dir.clone()),
            ..Self::new()
        };
        if v2_cache.try_load_v2(config).is_some() {
            cache = v2_cache;
        } else {
            cache.eviction_counters = v2_cache.eviction_counters;
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
            basename_alias_index: filter_basename_alias_index_for_live_keys(
                &self.basename_alias_index,
                &live_keys,
                &self.file_ref_index,
            ),
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

    pub fn persist_file_path(root: &Path) -> PathBuf {
        persist_cache_path_v2(root)
    }

    pub(crate) fn collect_live_entries(&self, ttl_secs: u64) -> Vec<PersistPacketCacheEntry> {
        let now = now_unix();
        self.entries_by_hash
            .values()
            .filter(|entry| !is_expired(entry.created_at_unix, ttl_secs, now))
            .map(PersistPacketCacheEntry::from_entry)
            .collect()
    }

    pub(crate) fn try_load_v2(&mut self, config: &PersistConfig) -> Option<()> {
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

    pub(crate) fn try_load_v1(&mut self, config: &PersistConfig) -> Option<()> {
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

pub(crate) fn persist_cache_path_v1(root: &Path) -> PathBuf {
    root.join(PERSIST_CACHE_DIR).join(PERSIST_CACHE_FILE_V1)
}

pub(crate) fn persist_cache_path_v2(root: &Path) -> PathBuf {
    root.join(PERSIST_CACHE_DIR).join(PERSIST_CACHE_FILE_V2)
}

pub(crate) fn filter_postings_for_live_keys(
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

pub(crate) fn filter_ref_index_for_live_keys(
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

pub(crate) fn filter_basename_alias_index_for_live_keys(
    index: &HashMap<String, BTreeSet<String>>,
    live_keys: &BTreeSet<String>,
    file_ref_index: &HashMap<String, BTreeSet<String>>,
) -> HashMap<String, BTreeSet<String>> {
    index
        .iter()
        .filter_map(|(basename, canonicals)| {
            let filtered = canonicals
                .iter()
                .filter(|canonical| {
                    file_ref_index
                        .get(*canonical)
                        .map(|cache_keys| {
                            cache_keys
                                .iter()
                                .any(|cache_key| live_keys.contains(cache_key))
                        })
                        .unwrap_or(false)
                })
                .cloned()
                .collect::<BTreeSet<_>>();
            (!filtered.is_empty()).then(|| (basename.clone(), filtered))
        })
        .collect()
}

pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), io::Error> {
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
