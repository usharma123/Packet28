//! Project-local filter trust system.
//!
//! Tracks which project-local filter scripts have been explicitly trusted
//! by the user. When a filter's content changes, trust is revoked until
//! the user re-approves.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Trust status for a project-local filter.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TrustStatus {
    /// Content hash matches the trusted version.
    Trusted,
    /// Content has changed since it was trusted.
    ContentChanged,
    /// Filter has never been trusted.
    Untrusted,
}

/// An entry in the trust store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustEntry {
    pub path: String,
    pub content_hash: String,
    pub trusted_at_unix: u64,
}

/// The trust store, persisted as JSON.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TrustStore {
    pub entries: BTreeMap<String, TrustEntry>,
}

/// Default path for the trust store.
pub fn default_trust_store_path() -> PathBuf {
    #[cfg(unix)]
    {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(format!("{home}/.local/share/packet28/trusted_filters.json"));
        }
    }
    PathBuf::from("/tmp/packet28/trusted_filters.json")
}

/// Load the trust store from disk.
pub fn load_trust_store(path: &Path) -> Result<TrustStore> {
    if !path.exists() {
        return Ok(TrustStore::default());
    }
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read trust store '{}'", path.display()))?;
    let store: TrustStore = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse trust store '{}'", path.display()))?;
    Ok(store)
}

/// Save the trust store to disk.
pub fn save_trust_store(path: &Path, store: &TrustStore) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory '{}'", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(store)?;
    fs::write(path, json)
        .with_context(|| format!("failed to write trust store '{}'", path.display()))?;
    Ok(())
}

/// Trust a filter at its current content hash.
pub fn trust_filter(store: &mut TrustStore, filter_path: &Path) -> Result<()> {
    let canonical = filter_path.display().to_string();
    let hash = compute_content_hash(filter_path)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    store.entries.insert(
        canonical.clone(),
        TrustEntry {
            path: canonical,
            content_hash: hash,
            trusted_at_unix: now,
        },
    );
    Ok(())
}

/// Check the trust status of a project-local filter.
pub fn verify_project_filter(store: &TrustStore, filter_path: &Path) -> Result<TrustStatus> {
    let canonical = filter_path.display().to_string();
    let Some(entry) = store.entries.get(&canonical) else {
        return Ok(TrustStatus::Untrusted);
    };
    let current_hash = compute_content_hash(filter_path)?;
    if current_hash == entry.content_hash {
        Ok(TrustStatus::Trusted)
    } else {
        Ok(TrustStatus::ContentChanged)
    }
}

/// Revoke trust for a filter.
pub fn revoke_trust(store: &mut TrustStore, filter_path: &Path) {
    let canonical = filter_path.display().to_string();
    store.entries.remove(&canonical);
}

fn compute_content_hash(path: &Path) -> Result<String> {
    let contents =
        fs::read(path).with_context(|| format!("failed to read '{}'", path.display()))?;
    Ok(blake3::hash(&contents).to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_and_verify_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let filter_path = dir.path().join("my-filter.toml");
        fs::write(&filter_path, "rule = \"test\"").unwrap();

        let mut store = TrustStore::default();
        trust_filter(&mut store, &filter_path).unwrap();

        assert_eq!(
            verify_project_filter(&store, &filter_path).unwrap(),
            TrustStatus::Trusted
        );
    }

    #[test]
    fn content_change_revokes_trust() {
        let dir = tempfile::tempdir().unwrap();
        let filter_path = dir.path().join("my-filter.toml");
        fs::write(&filter_path, "rule = \"original\"").unwrap();

        let mut store = TrustStore::default();
        trust_filter(&mut store, &filter_path).unwrap();

        fs::write(&filter_path, "rule = \"changed\"").unwrap();
        assert_eq!(
            verify_project_filter(&store, &filter_path).unwrap(),
            TrustStatus::ContentChanged
        );
    }

    #[test]
    fn untrusted_filter() {
        let dir = tempfile::tempdir().unwrap();
        let filter_path = dir.path().join("unknown-filter.toml");
        fs::write(&filter_path, "data").unwrap();

        let store = TrustStore::default();
        assert_eq!(
            verify_project_filter(&store, &filter_path).unwrap(),
            TrustStatus::Untrusted
        );
    }

    #[test]
    fn revoke_trust_removes_entry() {
        let dir = tempfile::tempdir().unwrap();
        let filter_path = dir.path().join("filter.toml");
        fs::write(&filter_path, "content").unwrap();

        let mut store = TrustStore::default();
        trust_filter(&mut store, &filter_path).unwrap();
        revoke_trust(&mut store, &filter_path);

        assert_eq!(
            verify_project_filter(&store, &filter_path).unwrap(),
            TrustStatus::Untrusted
        );
    }

    #[test]
    fn load_save_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("trust.json");
        let filter_path = dir.path().join("filter.toml");
        fs::write(&filter_path, "content").unwrap();

        let mut store = TrustStore::default();
        trust_filter(&mut store, &filter_path).unwrap();
        save_trust_store(&store_path, &store).unwrap();

        let loaded = load_trust_store(&store_path).unwrap();
        assert_eq!(loaded.entries.len(), 1);
    }
}
