use std::path::{Path, PathBuf};

pub fn current_repo_root_id(source_root: Option<&Path>) -> Option<String> {
    let cwd = std::env::current_dir().ok();
    let root = source_root
        .map(|p| p.to_path_buf())
        .or_else(|| cwd.as_deref().and_then(git_toplevel_from))
        .or(cwd)?;

    let canonical = root.canonicalize().ok().unwrap_or(root);
    let root_str = canonical.to_string_lossy();
    Some(blake3::hash(root_str.as_bytes()).to_hex().to_string()[..16].to_string())
}

fn git_toplevel_from(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}
