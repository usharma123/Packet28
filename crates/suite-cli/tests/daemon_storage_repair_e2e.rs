use std::collections::BTreeMap;
use std::fs;

use assert_cmd::Command;
use packet28_daemon_core::storage::{
    load_task_watch_registry_with_deltas_and_event_tails, save_task_watch_registry_checkpoint,
};
use packet28_daemon_protocol::paths::{task_event_log_path, task_events_dir, TaskStorageId};
use packet28_daemon_protocol::task::{TaskRecord, TaskRegistry, WatchRegistry};
use predicates::prelude::*;
use serde_json::{json, Value};
use tempfile::TempDir;

fn suite_cmd() -> Command {
    assert_cmd::cargo::cargo_bin_cmd!("Packet28")
}

fn seed_task_registry(root: &TempDir, tasks: &[(&str, u64)]) {
    let tasks = tasks
        .iter()
        .map(|(task_id, last_event_seq)| {
            (
                (*task_id).to_string(),
                TaskRecord {
                    task_id: (*task_id).to_string(),
                    last_event_seq: *last_event_seq,
                    ..TaskRecord::default()
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    save_task_watch_registry_checkpoint(
        root.path(),
        &TaskRegistry { tasks },
        &WatchRegistry::default(),
    )
    .unwrap();
}

fn corrupt_event_log(root: &TempDir, task_id: &str) -> std::path::PathBuf {
    let storage_id = TaskStorageId::try_from(task_id).unwrap();
    let path = task_event_log_path(root.path(), &storage_id);
    fs::create_dir_all(task_events_dir(root.path())).unwrap();
    fs::write(&path, b"{not-valid-json\n").unwrap();
    path
}

fn repair_json(root: &TempDir, extra_args: &[&str]) -> Value {
    let root_arg = root.path().to_str().unwrap();
    let mut command = suite_cmd();
    command.args(["daemon", "storage", "repair", "--root", root_arg, "--json"]);
    command.args(extra_args);
    let output = command.assert().success().get_output().stdout.clone();
    serde_json::from_slice(&output).unwrap()
}

#[test]
fn storage_repair_dry_run_reports_corruption_without_mutating_it() {
    let root = TempDir::new().unwrap();
    seed_task_registry(&root, &[("bad", 7), ("healthy", 0)]);
    let bad_log = corrupt_event_log(&root, "bad");
    let original = fs::read(&bad_log).unwrap();

    let report = repair_json(&root, &[]);

    assert_eq!(report["workspace_root"], root.path().display().to_string());
    assert_eq!(report["applied"], false);
    assert_eq!(report["corrupt_task_event_logs"], 1);
    assert_eq!(report["records"][0]["task_id"], "bad");
    assert_eq!(
        report["records"][0]["event_log_path"],
        bad_log.display().to_string()
    );
    assert!(report["records"][0]["quarantined_path"].is_null());
    assert!(report["records"][0]["reason"]
        .as_str()
        .is_some_and(|reason| reason.contains("event")));
    assert_eq!(fs::read(&bad_log).unwrap(), original);
    assert_eq!(
        fs::read_dir(task_events_dir(root.path())).unwrap().count(),
        1,
        "dry-run must not create a quarantine file"
    );

    suite_cmd()
        .args([
            "daemon",
            "storage",
            "repair",
            "--root",
            root.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "found (dry run) 1 corrupt task event log(s)",
        ))
        .stdout(predicate::str::contains("task_id=bad"))
        .stdout(predicate::str::contains("re-run with --apply"));
    assert_eq!(fs::read(&bad_log).unwrap(), original);
}

#[test]
fn storage_repair_apply_repairs_all_logs_and_is_idempotent() {
    let root = TempDir::new().unwrap();
    seed_task_registry(&root, &[("alpha", 3), ("beta", 9), ("healthy", 0)]);
    let alpha_log = corrupt_event_log(&root, "alpha");
    let beta_log = corrupt_event_log(&root, "beta");

    let report = repair_json(&root, &["--apply", "--pretty"]);

    assert_eq!(report["applied"], true);
    assert_eq!(report["corrupt_task_event_logs"], 2);
    assert_eq!(report["records"][0]["task_id"], "alpha");
    assert_eq!(report["records"][1]["task_id"], "beta");
    assert!(!alpha_log.exists());
    assert!(!beta_log.exists());
    for record in report["records"].as_array().unwrap() {
        let quarantine = record["quarantined_path"].as_str().unwrap();
        assert!(std::path::Path::new(quarantine).exists());
        assert!(quarantine.contains(".events.jsonl.corrupt-"));
    }

    let (loaded, tails) =
        load_task_watch_registry_with_deltas_and_event_tails(root.path()).unwrap();
    assert_eq!(loaded.tasks.tasks["alpha"].last_event_seq, 0);
    assert_eq!(loaded.tasks.tasks["beta"].last_event_seq, 0);
    assert_eq!(loaded.tasks.tasks["healthy"].last_event_seq, 0);
    assert_eq!(tails.get("alpha"), Some(&None));
    assert_eq!(tails.get("beta"), Some(&None));
    assert_eq!(tails.get("healthy"), Some(&None));

    let second_report = repair_json(&root, &["--apply"]);
    assert_eq!(second_report["applied"], true);
    assert_eq!(second_report["corrupt_task_event_logs"], 0);
    assert_eq!(second_report["records"], json!([]));
}

#[test]
fn storage_repair_on_clean_store_does_not_start_a_daemon() {
    let root = TempDir::new().unwrap();
    seed_task_registry(&root, &[("healthy", 0)]);

    let report = repair_json(&root, &[]);

    assert_eq!(report["applied"], false);
    assert_eq!(report["corrupt_task_event_logs"], 0);
    assert_eq!(report["records"], json!([]));
    assert!(!root.path().join(".packet28/daemon/runtime.json").exists());
    assert!(!root.path().join(".packet28/daemon/pid").exists());
    assert!(!root.path().join(".packet28/daemon/ready").exists());
}

#[test]
fn storage_repair_rejects_tampered_journal_paths_without_mutation() {
    let root = TempDir::new().unwrap();
    seed_task_registry(&root, &[("bad", 4)]);
    let bad_log = corrupt_event_log(&root, "bad");
    let original = fs::read(&bad_log).unwrap();
    let outside = root.path().join("outside-sentinel");
    fs::write(&outside, b"preserve me").unwrap();

    let journal_path = root
        .path()
        .join(".packet28/daemon/task-event-log-repair-v1.json");
    let journal = json!({
        "version": 1,
        "entries": [{
            "task_id": "bad",
            "event_log_path": "outside-sentinel",
            "quarantine_path": ".packet28/daemon/tasks/bad.events.jsonl.corrupt-1",
            "reason": "forged repair intent"
        }]
    });
    fs::write(&journal_path, serde_json::to_vec(&journal).unwrap()).unwrap();

    suite_cmd()
        .args([
            "daemon",
            "storage",
            "repair",
            "--root",
            root.path().to_str().unwrap(),
            "--apply",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "repair journal has an invalid path for task \"bad\"",
        ));

    assert_eq!(fs::read(&bad_log).unwrap(), original);
    assert_eq!(fs::read(&outside).unwrap(), b"preserve me");
    assert!(
        journal_path.exists(),
        "failed recovery intent must be retained"
    );
}
