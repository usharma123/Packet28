use std::collections::BTreeMap;
use std::path::Path;

use ignore::WalkBuilder;

use crate::error::CovyError;
use crate::model::RepoSnapshot;

/// Build a repository snapshot by hashing all tracked files.
pub fn build_snapshot(root: &Path) -> Result<RepoSnapshot, CovyError> {
    let mut file_hashes = BTreeMap::new();

    let walker = WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .filter_entry(|entry| {
            // Skip .git directory itself
            let name = entry.file_name().to_string_lossy();
            name != ".git"
        })
        .build();

    for entry in walker {
        let entry = entry.map_err(|e| CovyError::IoRaw(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        if !entry.file_type().map_or(false, |ft| ft.is_file()) {
            continue;
        }
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();

        let content = std::fs::read(path)?;
        let hash = blake3::hash(&content);
        file_hashes.insert(relative, hash.to_hex().to_string());
    }

    // Merkle root: hash of sorted (path, hash) pairs
    let mut hasher = blake3::Hasher::new();
    for (path, hash) in &file_hashes {
        hasher.update(path.as_bytes());
        hasher.update(hash.as_bytes());
    }
    let merkle_root = hasher.finalize().to_hex().to_string();

    Ok(RepoSnapshot {
        merkle_root,
        file_hashes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use std::fs;

    #[test]
    fn test_snapshot_basic() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.rs"), "fn main() {}").unwrap();
        fs::write(dir.path().join("b.rs"), "fn test() {}").unwrap();

        let snap = build_snapshot(dir.path()).unwrap();
        assert_eq!(snap.file_hashes.len(), 2);
        assert!(!snap.merkle_root.is_empty());
    }

    #[test]
    fn test_snapshot_deterministic() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.rs"), "hello").unwrap();
        fs::write(dir.path().join("b.rs"), "world").unwrap();

        let s1 = build_snapshot(dir.path()).unwrap();
        let s2 = build_snapshot(dir.path()).unwrap();
        assert_eq!(s1.merkle_root, s2.merkle_root);
    }

    #[test]
    fn test_snapshot_changes_on_edit() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.rs"), "v1").unwrap();
        let s1 = build_snapshot(dir.path()).unwrap();

        fs::write(dir.path().join("a.rs"), "v2").unwrap();
        let s2 = build_snapshot(dir.path()).unwrap();
        assert_ne!(s1.merkle_root, s2.merkle_root);
    }
}
