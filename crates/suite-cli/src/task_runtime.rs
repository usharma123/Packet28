use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use packet28_daemon_core::{active_task_path, ActiveTaskRecord};

pub fn load_active_task(root: &Path) -> Option<ActiveTaskRecord> {
    let path = active_task_path(root);
    fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<ActiveTaskRecord>(&raw).ok())
}

pub fn store_active_task(root: &Path, record: &ActiveTaskRecord) -> Result<()> {
    let path = active_task_path(root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create '{}'", parent.display()))?;
    }
    fs::write(&path, serde_json::to_vec_pretty(record)?)
        .with_context(|| format!("failed to write '{}'", path.display()))?;
    Ok(())
}

pub fn derive_claude_task_id(session_id: &str) -> String {
    crate::broker_client::derive_task_id(&format!("claude-session:{session_id}"))
}
