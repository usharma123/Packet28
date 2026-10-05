use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use packet28_daemon_core::storage::{
    load_active_task_record, load_task_registry, now_unix, save_active_task_record,
};
use packet28_daemon_protocol::hooks::ActiveTaskRecord;
use packet28_daemon_protocol::paths::task_registry_path;
use packet28_daemon_protocol::task::TaskRegistry;

pub fn load_active_task(root: &Path) -> Result<Option<ActiveTaskRecord>> {
    load_active_task_record(root).context("failed to load bounded active-task state")
}

pub fn store_active_task(root: &Path, record: &ActiveTaskRecord) -> Result<()> {
    save_active_task_record(root, record).context("failed to store bounded active-task state")
}

pub fn derive_claude_task_id(session_id: &str) -> String {
    crate::broker_client::derive_task_id(&format!("claude-session:{session_id}"))
}

/// Follows history-recovery links from `task_id` to the task continuing it.
///
/// A task whose event history was quarantined is fenced by the daemon; hooks
/// and MCP sessions that still derive or remember its identifier continue
/// under the linked successor instead. The daemon checkpoints every link
/// before readiness, so the checkpoint is sufficient once the daemon is
/// ready. A missing record ends the chain.
pub fn resolve_task_continuation(root: &Path, task_id: &str) -> Result<String> {
    if !task_registry_path(root).exists() {
        return Ok(task_id.to_string());
    }
    let registry = load_task_registry(root)
        .context("failed to load the task registry to resolve task continuation")?;
    continuation_in_registry(&registry, task_id)
}

fn continuation_in_registry(registry: &TaskRegistry, task_id: &str) -> Result<String> {
    let mut current = task_id;
    let mut visited = BTreeSet::new();
    while let Some(link) = registry
        .tasks
        .get(current)
        .and_then(|task| task.superseded_by.as_ref())
    {
        if !visited.insert(current) {
            return Err(anyhow!(
                "task recovery links for {task_id:?} form a cycle at {current:?}"
            ));
        }
        if link.predecessor_task_id != current
            || registry
                .tasks
                .get(&link.successor_task_id)
                .and_then(|task| task.recovered_from.as_ref())
                != Some(link)
        {
            return Err(anyhow!("invalid task recovery link for {current:?}"));
        }
        current = &link.successor_task_id;
    }
    Ok(current.to_string())
}

/// Returns immutable artifact namespaces, newest first, using reciprocal
/// registry links. Never follows a successor when reading an explicit owner.
pub fn artifact_task_lineage(root: &Path, task_id: &str) -> Result<Vec<String>> {
    let registry = load_task_registry(root)?;
    let mut owners = vec![task_id.to_string()];
    let mut current = task_id;
    while let Some(link) = registry
        .tasks
        .get(current)
        .and_then(|task| task.recovered_from.as_ref())
    {
        if link.successor_task_id != current
            || registry
                .tasks
                .get(&link.predecessor_task_id)
                .and_then(|task| task.superseded_by.as_ref())
                != Some(link)
            || owners.contains(&link.predecessor_task_id)
        {
            return Err(anyhow!("invalid artifact recovery lineage for {task_id:?}"));
        }
        owners.push(link.predecessor_task_id.clone());
        current = &link.predecessor_task_id;
    }
    Ok(owners)
}

/// Resolves `task_id` to its continuation and records a changed result as the
/// active task, so later runtime hooks address the successor directly.
pub fn adopt_task_continuation(
    root: &Path,
    task_id: String,
    session_id: Option<&str>,
) -> Result<String> {
    let resolved = resolve_task_continuation(root, &task_id)?;
    if resolved != task_id {
        store_active_task(
            root,
            &ActiveTaskRecord {
                task_id: resolved.clone(),
                session_id: session_id.map(ToOwned::to_owned),
                updated_at_unix: now_unix(),
            },
        )?;
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use packet28_daemon_core::storage::save_task_watch_registry_checkpoint;
    use packet28_daemon_protocol::task::{TaskHistoryRecovery, TaskRecord, WatchRegistry};

    fn linked(task_id: &str, successor: Option<&str>) -> (String, TaskRecord) {
        (
            task_id.to_string(),
            TaskRecord {
                task_id: task_id.to_string(),
                superseded_by: successor.map(|successor| TaskHistoryRecovery {
                    predecessor_task_id: task_id.to_string(),
                    successor_task_id: successor.to_string(),
                    ..TaskHistoryRecovery::default()
                }),
                ..TaskRecord::default()
            },
        )
    }

    #[test]
    fn continuation_follows_recovery_links_and_adopts_the_successor() {
        let root = tempdir().unwrap();
        assert_eq!(
            resolve_task_continuation(root.path(), "task").unwrap(),
            "task"
        );
        let mut registry = TaskRegistry {
            tasks: [
                linked("task", Some("task-recovered-1")),
                linked("task-recovered-1", Some("task-recovered-1-recovered-1")),
                linked("task-recovered-1-recovered-1", None),
                linked("healthy", None),
            ]
            .into(),
        };
        let links = registry
            .tasks
            .values()
            .filter_map(|task| task.superseded_by.clone())
            .collect::<Vec<_>>();
        for link in links {
            registry
                .tasks
                .get_mut(&link.successor_task_id)
                .unwrap()
                .recovered_from = Some(link.clone());
        }
        save_task_watch_registry_checkpoint(root.path(), &registry, &WatchRegistry::default())
            .unwrap();

        assert_eq!(
            resolve_task_continuation(root.path(), "healthy").unwrap(),
            "healthy"
        );
        assert_eq!(
            resolve_task_continuation(root.path(), "unknown").unwrap(),
            "unknown"
        );
        assert!(load_active_task(root.path()).unwrap().is_none());
        assert_eq!(
            adopt_task_continuation(root.path(), "task".to_string(), Some("session")).unwrap(),
            "task-recovered-1-recovered-1"
        );
        let active = load_active_task(root.path()).unwrap().unwrap();
        assert_eq!(active.task_id, "task-recovered-1-recovered-1");
        assert_eq!(active.session_id.as_deref(), Some("session"));
    }

    #[test]
    fn continuation_rejects_a_recovery_link_cycle() {
        let registry = TaskRegistry {
            tasks: [linked("a", Some("b")), linked("b", Some("a"))].into(),
        };
        assert!(continuation_in_registry(&registry, "a").is_err());
    }
    use packet28_daemon_core::storage::MAX_ACTIVE_TASK_RECORD_BYTES;
    use packet28_daemon_protocol::paths::active_task_path;
    use std::fs;
    use tempfile::tempdir;

    fn record_with_encoded_size(target_bytes: usize) -> ActiveTaskRecord {
        let mut record = ActiveTaskRecord {
            task_id: "bounded".to_string(),
            session_id: Some(String::new()),
            updated_at_unix: 1,
        };
        let base_bytes = serde_json::to_vec_pretty(&record).unwrap().len();
        let padding = target_bytes
            .checked_sub(base_bytes)
            .expect("target must fit the base active-task record");
        record.session_id = Some("x".repeat(padding));
        assert_eq!(
            serde_json::to_vec_pretty(&record).unwrap().len(),
            target_bytes
        );
        record
    }

    #[test]
    fn active_task_record_accepts_the_exact_shared_byte_limit() {
        let root = tempdir().unwrap();
        let record = record_with_encoded_size(MAX_ACTIVE_TASK_RECORD_BYTES);

        store_active_task(root.path(), &record).unwrap();
        let loaded = load_active_task(root.path()).unwrap().unwrap();

        assert_eq!(loaded.task_id, "bounded");
        assert_eq!(
            fs::metadata(active_task_path(root.path())).unwrap().len(),
            MAX_ACTIVE_TASK_RECORD_BYTES as u64
        );
    }

    #[test]
    fn oversized_active_task_record_preserves_the_existing_file() {
        let root = tempdir().unwrap();
        let existing = ActiveTaskRecord {
            task_id: "existing".to_string(),
            session_id: None,
            updated_at_unix: 1,
        };
        store_active_task(root.path(), &existing).unwrap();
        let path = active_task_path(root.path());
        let before = fs::read(&path).unwrap();
        let oversized = record_with_encoded_size(MAX_ACTIVE_TASK_RECORD_BYTES + 1);

        let error = store_active_task(root.path(), &oversized).unwrap_err();

        assert!(format!("{error:#}").contains("maximum supported size"));
        assert_eq!(fs::read(path).unwrap(), before);
        assert_eq!(
            load_active_task(root.path()).unwrap().unwrap().task_id,
            "existing"
        );
    }

    #[test]
    fn oversized_persisted_active_task_record_is_not_read() {
        let root = tempdir().unwrap();
        let path = active_task_path(root.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let file = fs::File::create(&path).unwrap();
        file.set_len(MAX_ACTIVE_TASK_RECORD_BYTES as u64 + 1)
            .unwrap();

        assert!(load_active_task(root.path()).is_err());
    }
}
