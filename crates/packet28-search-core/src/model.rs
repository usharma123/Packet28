//! Shared runtime and persisted-format model.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use memmap2::Mmap;
use regex::Regex;
use serde::{Deserialize, Serialize};

pub(crate) const REGEX_INDEX_SCHEMA_VERSION: u32 = 3;
pub(crate) const REGEX_DIR_NAME: &str = "regex-v1";
pub(crate) const MANIFEST_FILE_NAME: &str = "manifest.json";
pub(crate) const PREVIOUS_MANIFEST_FILE_NAME: &str = "manifest.previous.json";
pub(crate) const WRITER_LOCK_FILE_NAME: &str = ".regex-v1.writer.lock";
pub(crate) const GENERATION_HIGH_WATER_FILE_NAME: &str = ".regex-v1.generation-high-water";
pub(crate) const BASE_LOOKUP_FILE_NAME: &str = "base.lookup.dat";
pub(crate) const BASE_POSTINGS_FILE_NAME: &str = "base.postings.dat";
pub(crate) const BASE_DOCS_FILE_NAME: &str = "docs.dat";
pub(crate) const OVERLAY_LOOKUP_FILE_NAME: &str = "overlay.lookup.dat";
pub(crate) const OVERLAY_POSTINGS_FILE_NAME: &str = "overlay.postings.dat";
pub(crate) const OVERLAY_DOCS_FILE_NAME: &str = "overlay.docs.dat";
pub(crate) const OVERLAY_STATE_FILE_NAME: &str = "overlay.state.json";
pub(crate) const LOOKUP_ROW_BYTES: usize = 24;
pub(crate) const SHORT_GRAM_BYTES: usize = 2;
pub(crate) const MIN_GRAM_BYTES: usize = 3;
pub(crate) const MAX_GRAM_BYTES: usize = 24;
pub(crate) const MAX_LITERAL_COVER: usize = 8;
pub(crate) const MAX_INDEXED_FILE_BYTES: usize = 2 * 1024 * 1024;
pub(crate) const SEGMENT_DOC_BATCH_SIZE: usize = 256;
pub(crate) const SEGMENT_RECORD_BYTES: usize = 14;
pub(crate) const MAX_INDEX_VERIFY_CANDIDATES: usize = 1024;
pub(crate) const MAX_INDEX_VERIFY_NUMERATOR: usize = 1;
pub(crate) const MAX_INDEX_VERIFY_DENOMINATOR: usize = 2;
pub(crate) const POSITION_BUCKET_COUNT: usize = 16;
pub(crate) const OVERLAY_COMPACTION_SEGMENTS: usize = 8;

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
/// Persistent metadata describing the currently published regex index.
pub struct RegexIndexManifest {
    /// On-disk index schema version.
    pub schema_version: u32,
    /// Version of the gram weighting table used by the index.
    pub weight_table_version: u32,
    /// Monotonically increasing publication generation.
    pub generation: u64,
    /// Digest binding this manifest to its immutable generation record.
    ///
    /// Older persisted generations may omit this field and are authenticated
    /// when loaded before their next publication.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publication_fingerprint: Option<String>,
    /// Whether test files were included during the last full build.
    pub include_tests: bool,
    /// Persistent lifecycle status such as `building`, `ready`, or `corrupt`.
    pub status: String,
    /// Number of discovered files in the last full build.
    pub total_files: usize,
    /// Number of files represented in the base layer.
    pub indexed_files: usize,
    /// Number of files represented in the overlay layer.
    pub overlay_files: usize,
    /// Number of immutable overlay segments referenced by this generation.
    pub overlay_segments: usize,
    /// Digest binding the overlay ownership and tombstone state to this manifest.
    ///
    /// Older generation records may omit this field; those records are still
    /// subject to structural newest-owner validation when loaded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlay_state_digest: Option<String>,
    /// Git commit associated with the base layer, when available.
    pub base_commit: Option<String>,
    /// Git commit whose clean working tree was observed before and after the full rebuild.
    ///
    /// A missing value on a Git-backed index means the persisted generation
    /// cannot authenticate the current workspace contents unless
    /// [`Self::workspace_attested_commit`] is present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_clean_commit: Option<String>,
    /// Git commit whose dirty working tree was attested by the full rebuild.
    ///
    /// The generation record binds the content digest of every Git-dirty path
    /// observed unchanged before and after the build. A clean build records
    /// [`Self::workspace_clean_commit`] instead; a manifest declaring both is
    /// rejected. Binaries that predate this field cannot authenticate such a
    /// generation and treat it as unusable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_attested_commit: Option<String>,
    /// Reason the index cannot currently serve queries.
    pub stale_reason: Option<String>,
    /// Unix timestamp at which the latest build started.
    pub last_build_started_at_unix: Option<u64>,
    /// Unix timestamp at which the latest build completed.
    pub last_build_completed_at_unix: Option<u64>,
    /// Most recent build, validation, or loading failure.
    pub last_error: Option<String>,
}

impl RegexIndexManifest {
    /// Returns the Git commit whose workspace this generation authenticated.
    pub(crate) fn authenticated_workspace_commit(
        &self,
        base: &str,
    ) -> std::result::Result<&str, String> {
        let commit = match (
            self.workspace_clean_commit.as_deref(),
            self.workspace_attested_commit.as_deref(),
        ) {
            (Some(commit), None) | (None, Some(commit)) => commit,
            (None, None) => return Err(
                "workspace freshness could not be authenticated; rebuild the regex index from a stable Git working tree"
                    .to_string(),
            ),
            (Some(_), Some(_)) => return Err(
                "workspace freshness attestation declares both a clean and a dirty working tree"
                    .to_string(),
            ),
        };
        if commit != base {
            return Err(format!(
                "workspace freshness attestation does not match the indexed base commit (base={base}, attested={commit})"
            ));
        }
        Ok(commit)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct OverlayState {
    pub(crate) shadowed_paths: BTreeSet<String>,
    pub(crate) deleted_paths: BTreeSet<String>,
    pub(crate) owners: BTreeMap<String, u64>,
    /// Content digests for every Git-dirty path authenticated by the latest publication.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) workspace_entries: BTreeMap<String, String>,
}

#[derive(
    Debug, Clone, Serialize, Deserialize, Default, wincode::SchemaRead, wincode::SchemaWrite,
)]
#[serde(default)]
pub(crate) struct DocRecord {
    pub(crate) doc_id: u32,
    pub(crate) path: String,
    pub(crate) size: u64,
    pub(crate) mtime_secs: u64,
    pub(crate) fingerprint: String,
}

#[derive(Debug, Clone, Default)]
/// An immutable, validated search index generation and its public manifest.
pub struct RegexIndexRuntime {
    /// Metadata for the loaded or unavailable generation.
    pub manifest: RegexIndexManifest,
    pub(crate) loaded: Option<Arc<LoadedIndex>>,
    pub(crate) publication_fingerprint: Option<String>,
}

impl RegexIndexRuntime {
    /// Returns whether a validated base and every referenced overlay segment are available.
    pub fn is_loaded(&self) -> bool {
        self.loaded.is_some()
    }

    /// Returns true when two reader generations retain the same immutable base layer.
    pub fn shares_base_with(&self, other: &Self) -> bool {
        match (self.loaded.as_ref(), other.loaded.as_ref()) {
            (Some(left), Some(right)) => Arc::ptr_eq(&left.base, &right.base),
            _ => false,
        }
    }
}

#[derive(Debug)]
pub(crate) struct LoadedIndex {
    pub(crate) base: Arc<LoadedLayer>,
    pub(crate) base_files: LayerFiles,
    pub(crate) overlays: Vec<LoadedOverlaySegment>,
    pub(crate) overlay_state: OverlayState,
}

#[derive(Debug, Clone)]
pub(crate) struct LoadedOverlaySegment {
    pub(crate) generation: u64,
    pub(crate) layer: Arc<LoadedLayer>,
    pub(crate) files: LayerFiles,
}

#[derive(Debug)]
pub(crate) struct LoadedLayer {
    pub(crate) docs: Vec<DocRecord>,
    pub(crate) doc_ids_by_path: HashMap<String, u32>,
    pub(crate) lookup: Option<Mmap>,
    pub(crate) postings: Option<Mmap>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SparseCandidate {
    pub(crate) hash: u64,
    pub(crate) score: u32,
    pub(crate) start: usize,
    pub(crate) end: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SearchPlan {
    All,
    Literal(Vec<u8>),
    And(Vec<SearchPlan>),
    Or(Vec<SearchPlan>),
}

impl SearchPlan {
    pub(crate) fn kind_str(&self) -> &'static str {
        match self {
            Self::All => "prefiltered_all",
            Self::Literal(_) => "literal",
            Self::And(_) => "and",
            Self::Or(_) => "or",
        }
    }
}

#[derive(Clone)]
pub(crate) struct CompiledSearch {
    pub(crate) verifier: Verifier,
    pub(crate) plan: SearchPlan,
    pub(crate) plan_kind: String,
    pub(crate) planner_fallback: Option<String>,
    pub(crate) must_fallback_reason: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct HeapItem {
    pub(crate) hash: u64,
    pub(crate) doc_id: u32,
    pub(crate) summary: PositionSummary,
    pub(crate) segment_idx: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct PositionSummary {
    pub(crate) buckets: u8,
    pub(crate) repeated: bool,
}

impl PositionSummary {
    pub(crate) fn new(bucket: u8) -> Self {
        Self {
            buckets: ((bucket & 0x0f) << 4) | (bucket & 0x0f),
            repeated: false,
        }
    }

    pub(crate) fn first_bucket(self) -> u8 {
        self.buckets >> 4
    }

    pub(crate) fn last_bucket(self) -> u8 {
        self.buckets & 0x0f
    }

    pub(crate) fn repeated(self) -> bool {
        self.repeated
    }

    pub(crate) fn update(&mut self, bucket: u8) {
        let bucket = bucket & 0x0f;
        let first = self.first_bucket().min(bucket);
        let last = self.last_bucket().max(bucket);
        self.buckets = (first << 4) | last;
        self.repeated = true;
    }

    pub(crate) fn merge(&mut self, other: PositionSummary) {
        let first = self.first_bucket().min(other.first_bucket());
        let last = self.last_bucket().max(other.last_bucket());
        self.buckets = (first << 4) | last;
        self.repeated = true;
        if other.repeated {
            self.repeated = true;
        }
    }

    pub(crate) fn encode(self) -> [u8; 2] {
        [self.buckets, u8::from(self.repeated)]
    }

    pub(crate) fn decode(bytes: [u8; 2]) -> Self {
        Self {
            buckets: bytes[0],
            repeated: bytes[1] != 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PostingEntry {
    pub(crate) doc_id: u32,
    pub(crate) summary: PositionSummary,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LookupPostingMeta {
    pub(crate) offset: u64,
    pub(crate) len: u32,
    pub(crate) doc_count: u32,
}

pub(crate) type PostingRow = (u64, u64, u32, u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LiteralWindow {
    pub(crate) earliest_bucket: u8,
    pub(crate) latest_bucket: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct IndexedGram {
    pub(crate) hash: u64,
    pub(crate) summary: PositionSummary,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum LayerKind {
    Base,
    Overlay(u64),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct LayerFiles {
    pub(crate) lookup: String,
    pub(crate) postings: String,
    pub(crate) docs: String,
    #[serde(default)]
    pub(crate) lookup_digest: String,
    #[serde(default)]
    pub(crate) postings_digest: String,
    #[serde(default)]
    pub(crate) docs_digest: String,
}

impl LayerFiles {
    pub(crate) fn legacy_base() -> Self {
        Self {
            lookup: BASE_LOOKUP_FILE_NAME.to_string(),
            postings: BASE_POSTINGS_FILE_NAME.to_string(),
            docs: BASE_DOCS_FILE_NAME.to_string(),
            lookup_digest: String::new(),
            postings_digest: String::new(),
            docs_digest: String::new(),
        }
    }

    pub(crate) fn legacy_overlay() -> Self {
        Self {
            lookup: OVERLAY_LOOKUP_FILE_NAME.to_string(),
            postings: OVERLAY_POSTINGS_FILE_NAME.to_string(),
            docs: OVERLAY_DOCS_FILE_NAME.to_string(),
            lookup_digest: String::new(),
            postings_digest: String::new(),
            docs_digest: String::new(),
        }
    }

    pub(crate) fn base(generation: u64) -> Self {
        Self {
            lookup: format!("base-{generation:020}.lookup.dat"),
            postings: format!("base-{generation:020}.postings.dat"),
            docs: format!("base-{generation:020}.docs.dat"),
            lookup_digest: String::new(),
            postings_digest: String::new(),
            docs_digest: String::new(),
        }
    }

    pub(crate) fn overlay(generation: u64, compacted: bool) -> Self {
        let suffix = if compacted { "-compacted" } else { "" };
        Self {
            lookup: format!("overlay-{generation:020}{suffix}.lookup.dat"),
            postings: format!("overlay-{generation:020}{suffix}.postings.dat"),
            docs: format!("overlay-{generation:020}{suffix}.docs.dat"),
            lookup_digest: String::new(),
            postings_digest: String::new(),
            docs_digest: String::new(),
        }
    }

    pub(crate) fn has_digests(&self) -> bool {
        !self.lookup_digest.is_empty()
            && !self.postings_digest.is_empty()
            && !self.docs_digest.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct OverlaySegmentRecord {
    pub(crate) generation: u64,
    pub(crate) files: LayerFiles,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RegexGenerationRecord {
    pub(crate) schema_version: u32,
    pub(crate) generation: u64,
    pub(crate) manifest: RegexIndexManifest,
    pub(crate) base: LayerFiles,
    pub(crate) segments: Vec<OverlaySegmentRecord>,
    pub(crate) overlay_state: OverlayState,
}

#[derive(Default)]
pub(crate) struct QueryCache {
    pub(crate) postings: HashMap<(LayerKind, u64), Option<Vec<PostingEntry>>>,
    pub(crate) literal_candidates: HashMap<Vec<u8>, BTreeSet<String>>,
    pub(crate) literal_hashes: HashMap<Vec<u8>, Vec<u64>>,
    pub(crate) literal_repeat_requirements: HashMap<Vec<u8>, HashMap<u64, bool>>,
    pub(crate) literal_windows: HashMap<(String, Vec<u8>), Option<LiteralWindow>>,
}

#[derive(Clone)]
pub(crate) enum Verifier {
    Regex {
        regex: Regex,
        whole_file_prefilter: bool,
    },
    FixedBytes {
        needle: Vec<u8>,
        case_insensitive: bool,
    },
}
