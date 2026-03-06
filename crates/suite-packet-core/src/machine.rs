use std::fmt::{Display, Formatter};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{CovyError, EnvelopeV1};

pub const MACHINE_SCHEMA_VERSION: &str = "suite.packet.v1";
pub const ARTIFACT_DIR: &str = ".packet28/artifacts";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum JsonProfile {
    #[default]
    Compact,
    Full,
    Handle,
}

impl Display for JsonProfile {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            JsonProfile::Compact => "compact",
            JsonProfile::Full => "full",
            JsonProfile::Handle => "handle",
        };
        write!(f, "{value}")
    }
}

impl FromStr for JsonProfile {
    type Err = CovyError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "compact" => Ok(Self::Compact),
            "full" => Ok(Self::Full),
            "handle" => Ok(Self::Handle),
            other => Err(CovyError::Config(format!(
                "invalid json profile '{other}', expected compact|full|handle"
            ))),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PacketWrapperV1<T> {
    pub schema_version: String,
    pub packet_type: String,
    pub cache_hit: bool,
    pub packet: T,
}

impl<T: Default> Default for PacketWrapperV1<T> {
    fn default() -> Self {
        Self {
            schema_version: MACHINE_SCHEMA_VERSION.to_string(),
            packet_type: String::new(),
            cache_hit: false,
            packet: T::default(),
        }
    }
}

impl<T> PacketWrapperV1<T> {
    pub fn new(packet_type: impl Into<String>, packet: T) -> Self {
        Self {
            schema_version: MACHINE_SCHEMA_VERSION.to_string(),
            packet_type: packet_type.into(),
            cache_hit: false,
            packet,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct ArtifactHandle {
    pub handle_id: String,
    pub packet_type: String,
    pub packet_hash: String,
    pub artifact_sha256: String,
    pub path: String,
    pub created_at_unix: u64,
}

pub fn wrap_envelope<T>(
    packet_type: impl Into<String>,
    envelope: EnvelopeV1<T>,
) -> PacketWrapperV1<EnvelopeV1<T>> {
    PacketWrapperV1::new(packet_type, envelope)
}

pub fn artifact_store_root(root: &Path) -> PathBuf {
    root.join(ARTIFACT_DIR)
}

pub fn artifact_path(root: &Path, handle_id: &str) -> PathBuf {
    artifact_store_root(root).join(format!("{handle_id}.json"))
}

pub fn write_packet_artifact<T: Serialize + Clone>(
    root: &Path,
    packet_type: &str,
    envelope: &EnvelopeV1<T>,
) -> Result<ArtifactHandle, CovyError> {
    let wrapper = wrap_envelope(packet_type.to_string(), envelope.clone());
    let json = serde_json::to_vec(&wrapper).map_err(|source| CovyError::Parse {
        format: "packet-json".to_string(),
        detail: source.to_string(),
    })?;

    let mut hasher = Sha256::new();
    hasher.update(&json);
    let artifact_sha256 = format!("{:x}", hasher.finalize());
    let created_at_unix = now_unix();
    let hash_prefix = envelope.hash.chars().take(16).collect::<String>();
    let handle_id = format!("{hash_prefix}-{created_at_unix}");

    let path = artifact_path(root, &handle_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| CovyError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(&path, json).map_err(|source| CovyError::Io {
        path: path.clone(),
        source,
    })?;

    Ok(ArtifactHandle {
        handle_id,
        packet_type: packet_type.to_string(),
        packet_hash: envelope.hash.clone(),
        artifact_sha256,
        path: path.to_string_lossy().to_string(),
        created_at_unix,
    })
}

pub fn read_packet_artifact(root: &Path, handle_id: &str) -> Result<serde_json::Value, CovyError> {
    let path = artifact_path(root, handle_id);
    let raw = std::fs::read_to_string(&path).map_err(|source| CovyError::Io {
        path: path.clone(),
        source,
    })?;
    serde_json::from_str(&raw).map_err(|source| CovyError::Parse {
        format: "packet-json".to_string(),
        detail: source.to_string(),
    })
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BudgetCost, Provenance};
    use serde_json::json;

    fn temp_root() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("packet28-machine-tests-{nanos}"));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn test_envelope() -> EnvelopeV1<serde_json::Value> {
        EnvelopeV1 {
            version: "1".to_string(),
            tool: "suite".to_string(),
            kind: "demo".to_string(),
            hash: String::new(),
            summary: "demo packet".to_string(),
            files: vec![],
            symbols: vec![],
            risk: None,
            confidence: Some(1.0),
            budget_cost: BudgetCost {
                est_tokens: 0,
                est_bytes: 0,
                runtime_ms: 0,
                tool_calls: 1,
                payload_est_tokens: None,
                payload_est_bytes: None,
            },
            provenance: Provenance {
                inputs: vec!["input.json".to_string()],
                git_base: None,
                git_head: None,
                generated_at_unix: now_unix(),
            },
            payload: json!({"ok": true, "items": [1, 2, 3]}),
        }
        .with_canonical_hash_and_real_budget()
    }

    #[test]
    fn write_and_read_packet_artifact_roundtrip() {
        let root = temp_root();
        let envelope = test_envelope();
        let handle = write_packet_artifact(&root, "suite.demo.packet.v1", &envelope).unwrap();

        assert_eq!(handle.packet_type, "suite.demo.packet.v1");
        assert_eq!(handle.packet_hash, envelope.hash);
        assert!(Path::new(&handle.path).exists());

        let read_back = read_packet_artifact(&root, &handle.handle_id).unwrap();
        let parsed: PacketWrapperV1<EnvelopeV1<serde_json::Value>> =
            serde_json::from_value(read_back).unwrap();
        assert_eq!(parsed.schema_version, MACHINE_SCHEMA_VERSION);
        assert_eq!(parsed.packet_type, "suite.demo.packet.v1");
        assert_eq!(parsed.packet.hash, envelope.hash);

        std::fs::remove_dir_all(root).ok();
    }
}
