use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

pub fn cache_fingerprint(repo_root: &Path, relevant_paths: &[PathBuf]) -> String {
    git_fingerprint(repo_root).unwrap_or_else(|| filesystem_fingerprint(repo_root, relevant_paths))
}

fn git_fingerprint(repo_root: &Path) -> Option<String> {
    let repo_root = canonical_or_original(repo_root);
    let top_level = git_output(&repo_root, &["rev-parse", "--show-toplevel"])?;
    let head = git_output(&repo_root, &["rev-parse", "HEAD"])?;
    let status = git_output(
        &repo_root,
        &["status", "--porcelain", "--untracked-files=no"],
    )?;

    let mut modified_paths = status
        .lines()
        .filter_map(parse_status_path)
        .collect::<Vec<_>>();
    modified_paths.sort();
    modified_paths.dedup();

    let mut hasher = blake3::Hasher::new();
    hasher.update(top_level.trim().as_bytes());
    hasher.update(head.trim().as_bytes());
    hasher.update(if modified_paths.is_empty() {
        b"clean"
    } else {
        b"dirty"
    });
    for path in modified_paths {
        hasher.update(path.as_bytes());
        if let Ok(metadata) = std::fs::metadata(repo_root.join(&path)) {
            hash_metadata(&mut hasher, &metadata);
        }
    }

    Some(hasher.finalize().to_hex().to_string())
}

fn filesystem_fingerprint(repo_root: &Path, relevant_paths: &[PathBuf]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(
        canonical_or_original(repo_root)
            .to_string_lossy()
            .as_bytes(),
    );

    let mut paths = if relevant_paths.is_empty() {
        collect_repo_files(repo_root)
    } else {
        relevant_paths
            .iter()
            .map(|path| {
                if path.is_absolute() {
                    path.clone()
                } else {
                    repo_root.join(path)
                }
            })
            .collect::<Vec<_>>()
    };
    paths.sort();
    paths.dedup();

    for path in paths {
        hasher.update(path.to_string_lossy().as_bytes());
        if let Ok(metadata) = std::fs::metadata(&path) {
            hash_metadata(&mut hasher, &metadata);
        }
    }

    hasher.finalize().to_hex().to_string()
}

fn collect_repo_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name == ".git" || name == ".packet28")
            {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                files.push(path);
            }
        }
    }
    files
}

fn parse_status_path(line: &str) -> Option<String> {
    if line.len() < 4 {
        return None;
    }
    let path = line[3..].trim();
    if path.is_empty() {
        return None;
    }
    if let Some((_, renamed)) = path.split_once(" -> ") {
        Some(renamed.to_string())
    } else {
        Some(path.to_string())
    }
}

fn hash_metadata(hasher: &mut blake3::Hasher, metadata: &std::fs::Metadata) {
    hasher.update(&metadata.len().to_le_bytes());
    if let Ok(modified) = metadata.modified() {
        if let Ok(duration) = modified.duration_since(UNIX_EPOCH) {
            hasher.update(&duration.as_secs().to_le_bytes());
            hasher.update(&duration.subsec_nanos().to_le_bytes());
        }
    }
}

fn git_output(repo_root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn canonical_or_original(path: &Path) -> PathBuf {
    path.canonicalize()
        .ok()
        .unwrap_or_else(|| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filesystem_fingerprint_changes_when_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("input.txt");
        std::fs::write(&file, "one").unwrap();

        let before = cache_fingerprint(dir.path(), std::slice::from_ref(&file));
        std::fs::write(&file, "two").unwrap();
        let after = cache_fingerprint(dir.path(), std::slice::from_ref(&file));

        assert_ne!(before, after);
    }
}
