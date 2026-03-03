use crate::error::CovyError;
use crate::testmap::TestMapIndex;

pub const TESTMAP_SCHEMA_VERSION: u16 = 2;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
