use crate::error::CovyError;
use crate::testmap::TestTimingHistory;

pub const TESTTIMINGS_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct StoredTestTimingHistory {
    schema_version: u16,
    timings: TestTimingHistory,
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

#[cfg(test)]
mod tests {
    use super::*;

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
