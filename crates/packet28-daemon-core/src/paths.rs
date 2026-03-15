use super::*;

pub fn daemon_dir(root: &Path) -> PathBuf {
    root.join(DAEMON_DIR_NAME)
}

fn socket_dir() -> PathBuf {
    std::env::temp_dir().join(SOCKET_DIR_NAME)
}

fn socket_file_name(root: &Path) -> String {
    let digest = blake3::hash(root.to_string_lossy().as_bytes()).to_hex();
    format!("p28-{}.sock", &digest[..16])
}

pub fn index_dir(root: &Path) -> PathBuf {
    root.join(INDEX_DIR_NAME)
}

pub fn index_manifest_path(root: &Path) -> PathBuf {
    index_dir(root).join(INDEX_MANIFEST_FILE_NAME)
}

pub fn index_snapshot_path(root: &Path) -> PathBuf {
    index_dir(root).join(INDEX_SNAPSHOT_FILE_NAME)
}

pub fn socket_path(root: &Path) -> PathBuf {
    socket_dir().join(socket_file_name(root))
}

pub fn pid_path(root: &Path) -> PathBuf {
    daemon_dir(root).join(PID_FILE_NAME)
}

pub fn runtime_path(root: &Path) -> PathBuf {
    daemon_dir(root).join(RUNTIME_FILE_NAME)
}

pub fn ready_path(root: &Path) -> PathBuf {
    daemon_dir(root).join(READY_FILE_NAME)
}

pub fn log_path(root: &Path) -> PathBuf {
    daemon_dir(root).join(LOG_FILE_NAME)
}

pub fn watch_registry_path(root: &Path) -> PathBuf {
    daemon_dir(root).join(WATCH_REGISTRY_FILE_NAME)
}

pub fn task_registry_path(root: &Path) -> PathBuf {
    daemon_dir(root).join(TASK_REGISTRY_FILE_NAME)
}

pub fn task_events_dir(root: &Path) -> PathBuf {
    daemon_dir(root).join(TASK_EVENTS_DIR_NAME)
}

pub fn task_artifacts_dir(root: &Path) -> PathBuf {
    root.join(".packet28").join(TASK_ARTIFACTS_DIR_NAME)
}

pub fn agent_runtime_dir(root: &Path) -> PathBuf {
    root.join(".packet28").join("agent")
}

pub fn hook_runtime_config_path(root: &Path) -> PathBuf {
    daemon_dir(root).join(HOOK_RUNTIME_CONFIG_FILE_NAME)
}

pub fn active_task_path(root: &Path) -> PathBuf {
    agent_runtime_dir(root).join(AGENT_ACTIVE_TASK_FILE_NAME)
}

pub fn task_event_log_path(root: &Path, task_id: &str) -> PathBuf {
    let safe = safe_task_id(task_id);
    task_events_dir(root).join(format!("{safe}.events.jsonl"))
}

pub fn task_artifact_dir(root: &Path, task_id: &str) -> PathBuf {
    task_artifacts_dir(root).join(safe_task_id(task_id))
}

pub fn task_brief_markdown_path(root: &Path, task_id: &str) -> PathBuf {
    task_artifact_dir(root, task_id).join(TASK_BRIEF_MARKDOWN_FILE_NAME)
}

pub fn task_brief_json_path(root: &Path, task_id: &str) -> PathBuf {
    task_artifact_dir(root, task_id).join(TASK_BRIEF_JSON_FILE_NAME)
}

pub fn task_state_json_path(root: &Path, task_id: &str) -> PathBuf {
    task_artifact_dir(root, task_id).join(TASK_STATE_JSON_FILE_NAME)
}

pub fn task_versions_dir(root: &Path, task_id: &str) -> PathBuf {
    task_artifact_dir(root, task_id).join("versions")
}

pub fn task_version_json_path(root: &Path, task_id: &str, context_version: &str) -> PathBuf {
    task_versions_dir(root, task_id).join(format!("{}.json", safe_task_id(context_version)))
}

fn safe_task_id(task_id: &str) -> String {
    let safe = task_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if safe.is_empty() {
        "task".to_string()
    } else {
        safe
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn socket_path_uses_short_hashed_temp_location() {
        let dir = tempdir().unwrap();
        let root = dir
            .path()
            .join("very")
            .join("long")
            .join("nested")
            .join("workspace")
            .join("path");
        let socket = socket_path(&root);

        assert!(socket.starts_with(std::env::temp_dir()));
        assert_eq!(
            socket.extension().and_then(|ext| ext.to_str()),
            Some("sock")
        );
        assert!(socket.to_string_lossy().len() < 104);
        assert_ne!(socket, daemon_dir(&root).join(SOCKET_FILE_NAME));
    }
}
