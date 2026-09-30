use packet28_daemon_core::storage::{
    load_task_watch_registry_recovering_corrupt_event_logs, load_task_watch_registry_with_deltas,
    save_task_watch_registry_checkpoint,
};
use packet28_daemon_protocol::paths::{task_event_log_path, task_events_dir, TaskStorageId};
use packet28_daemon_protocol::task::{TaskRecord, TaskRegistry, WatchRegistry};

#[test]
fn astra_maximum_valid_id_supports_corrupt_event_recovery() {
    let root = tempfile::tempdir().unwrap();
    let id = "a".repeat(242);
    let storage_id = TaskStorageId::try_from(id.as_str()).unwrap();
    let task = TaskRecord {
        task_id: id.clone(),
        last_event_seq: 7,
        ..Default::default()
    };
    save_task_watch_registry_checkpoint(
        root.path(),
        &TaskRegistry {
            tasks: [(id.clone(), task)].into(),
        },
        &WatchRegistry::default(),
    )
    .unwrap();
    std::fs::create_dir_all(task_events_dir(root.path())).unwrap();
    let log = task_event_log_path(root.path(), &storage_id);
    std::fs::write(&log, b"bad-json\n").unwrap();
    let result = load_task_watch_registry_recovering_corrupt_event_logs(root.path())
        .map(|(_, _, records)| records);
    let records = result.expect("valid maximum-length task ID must be recoverable");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].task_id, id);
    let destination = records[0].quarantined_path.as_ref().unwrap();
    assert!(destination.file_name().unwrap().as_encoded_bytes().len() <= 255);
    assert_eq!(std::fs::read(destination).unwrap(), b"bad-json\n");
    assert!(!log.exists());
    let loaded = load_task_watch_registry_with_deltas(root.path()).unwrap();
    assert_eq!(loaded.tasks.tasks[&id].last_event_seq, 0);
    assert!(
        load_task_watch_registry_recovering_corrupt_event_logs(root.path())
            .unwrap()
            .2
            .is_empty()
    );
}

#[test]
fn astra_corrupt_checkpoint_task_recovers_alongside_wal_only_admission() {
    use packet28_daemon_core::storage::{
        append_task_watch_registry_delta, RegistryDeltaBatch, RegistryRevision,
        RegistryRevisionRange,
    };
    let root = tempfile::tempdir().unwrap();
    let bad = TaskRecord {
        task_id: "bad".into(),
        last_event_seq: 7,
        ..Default::default()
    };
    save_task_watch_registry_checkpoint(
        root.path(),
        &TaskRegistry {
            tasks: [("bad".into(), bad)].into(),
        },
        &WatchRegistry::default(),
    )
    .unwrap();
    let new = TaskRecord {
        task_id: "wal-only".into(),
        ..Default::default()
    };
    append_task_watch_registry_delta(
        root.path(),
        RegistryRevisionRange::single(RegistryRevision::new(1)).unwrap(),
        &RegistryDeltaBatch::default().upsert_task(new),
    )
    .unwrap();
    std::fs::create_dir_all(task_events_dir(root.path())).unwrap();
    let log = task_event_log_path(root.path(), &TaskStorageId::try_from("bad").unwrap());
    std::fs::write(&log, b"bad-json\n").unwrap();
    let result = load_task_watch_registry_recovering_corrupt_event_logs(root.path());
    let (loaded, _, quarantined) =
        result.expect("WAL-admitted healthy task must not block recovery");
    assert!(loaded.tasks.tasks.contains_key("wal-only"));
    assert_eq!(loaded.tasks.tasks["bad"].last_event_seq, 0);
    assert_eq!(quarantined.len(), 1);
    assert_eq!(quarantined[0].task_id, "bad");
    assert_eq!(
        std::fs::read(quarantined[0].quarantined_path.as_ref().unwrap()).unwrap(),
        b"bad-json\n"
    );
}
