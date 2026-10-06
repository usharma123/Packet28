use std::collections::BTreeMap;
use std::fs;

use assert_cmd::Command;
use packet28_daemon_core::storage::{
    append_next_task_event, load_task_watch_registry_with_deltas_and_event_tails,
    save_task_watch_registry_checkpoint,
};
use packet28_daemon_protocol::message::DaemonEvent;
use packet28_daemon_protocol::paths::{task_event_log_path, task_events_dir, TaskStorageId};
use packet28_daemon_protocol::task::{TaskLifecycle, TaskRecord, TaskRegistry, WatchRegistry};
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
    for (task_id, prior_seq) in [("alpha", 3), ("beta", 9)] {
        let predecessor = &loaded.tasks.tasks[task_id];
        let link = predecessor.superseded_by.as_ref().unwrap();
        assert_eq!(predecessor.lifecycle, TaskLifecycle::Cancelled);
        assert_eq!(link.prior_last_event_seq, prior_seq);
        assert_eq!(
            loaded.tasks.tasks[&link.successor_task_id]
                .recovered_from
                .as_ref(),
            Some(link)
        );
        let reported = report["records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|record| record["task_id"] == task_id)
            .unwrap();
        assert_eq!(reported["successor_task_id"], link.successor_task_id);
        let event = DaemonEvent {
            kind: "after-repair".to_string(),
            occurred_at_unix: 1,
            data: Value::Null,
        };
        assert!(append_next_task_event(root.path(), task_id, &event).is_err());
        assert!(
            !task_event_log_path(root.path(), &TaskStorageId::try_from(task_id).unwrap()).exists()
        );
        assert_eq!(
            append_next_task_event(root.path(), &link.successor_task_id, &event)
                .unwrap()
                .seq,
            1
        );
    }

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
fn storage_repair_rejects_legacy_journal_without_mutation() {
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
            "legacy repair journal requires manual inspection",
        ));

    assert_eq!(fs::read(&bad_log).unwrap(), original);
    assert_eq!(fs::read(&outside).unwrap(), b"preserve me");
    assert!(
        journal_path.exists(),
        "failed recovery intent must be retained"
    );
}

#[cfg(unix)]
#[test]
fn storage_repair_refuses_daemon_startup_before_a_socket_exists() {
    let root = TempDir::new().unwrap();
    seed_task_registry(&root, &[("bad", 4)]);
    let bad_log = corrupt_event_log(&root, "bad");
    let _instance =
        packet28_daemon_core::task_store_lease::acquire_daemon_instance_lease(root.path()).unwrap();
    for apply in [false, true] {
        let mut command = suite_cmd();
        command.args([
            "daemon",
            "storage",
            "repair",
            "--root",
            root.path().to_str().unwrap(),
        ]);
        if apply {
            command.arg("--apply");
        }
        command
            .timeout(std::time::Duration::from_secs(5))
            .assert()
            .failure()
            .stderr(predicate::str::contains("exclusive task-store access"));
        assert_eq!(fs::read(&bad_log).unwrap(), b"{not-valid-json\n");
        assert_eq!(
            fs::read_dir(task_events_dir(root.path())).unwrap().count(),
            1
        );
    }
}

fn whitespace_edit_task_registry(root: &TempDir) -> (std::path::PathBuf, Vec<u8>) {
    let path = root.path().join(".packet28/daemon/task-registry-v1.json");
    let committed = fs::read(&path).unwrap();
    fs::write(&path, [committed.as_slice(), b" \n"].concat()).unwrap();
    (path, committed)
}

#[test]
fn storage_repair_restores_a_whitespace_edited_registry_exactly() {
    let root = TempDir::new().unwrap();
    seed_task_registry(&root, &[("task", 0), ("healthy", 0)]);
    let bad_log = corrupt_event_log(&root, "healthy");
    let (task_path, committed) = whitespace_edit_task_registry(&root);
    let edited = fs::read(&task_path).unwrap();

    let dry = repair_json(&root, &[]);
    assert_eq!(dry["registry_checkpoint"]["status"], "repairable");
    assert_eq!(
        dry["registry_checkpoint"]["authority"],
        "committed_manifest"
    );
    assert_eq!(
        dry["registry_checkpoint"]["files"][0]["source"],
        "canonical_reencoding"
    );
    assert!(dry["corrupt_task_event_logs"].is_null());
    assert_eq!(fs::read(&task_path).unwrap(), edited);

    suite_cmd()
        .args(["daemon", "storage", "repair", "--root"])
        .arg(root.path())
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "exact checkpoint restore available (dry run)",
        ))
        .stdout(predicate::str::contains(
            "task event logs were not inspected",
        ))
        .stdout(predicate::str::contains("re-run with --apply"));

    let applied = repair_json(&root, &["--apply"]);
    assert_eq!(applied["registry_checkpoint"]["status"], "repaired");
    assert_eq!(
        applied["registry_checkpoint"]["files"][0]["state"],
        "restored"
    );
    let archive = std::path::PathBuf::from(
        applied["registry_checkpoint"]["archive_path"]
            .as_str()
            .unwrap(),
    );
    assert_eq!(
        fs::read(archive.join("original-task-registry-v1.json")).unwrap(),
        edited
    );
    assert_eq!(fs::read(&task_path).unwrap(), committed);
    // The existing event-log quarantine flow runs once authority is restored.
    assert_eq!(applied["corrupt_task_event_logs"], 1);
    assert!(!bad_log.exists());
    let (loaded, _) = load_task_watch_registry_with_deltas_and_event_tails(root.path()).unwrap();
    assert!(loaded.tasks.tasks.contains_key("task"));

    let clean = repair_json(&root, &["--apply"]);
    assert_eq!(clean["registry_checkpoint"]["status"], "clean");
}

#[test]
fn storage_repair_reports_no_safe_recovery_and_changes_nothing() {
    let root = TempDir::new().unwrap();
    seed_task_registry(&root, &[("task", 0)]);
    let task_path = root.path().join(".packet28/daemon/task-registry-v1.json");
    let mut value: Value = serde_json::from_slice(&fs::read(&task_path).unwrap()).unwrap();
    value["tasks"]["task"]["last_event_seq"] = json!(9);
    let edited = serde_json::to_vec_pretty(&value).unwrap();
    fs::write(&task_path, &edited).unwrap();

    let output = suite_cmd()
        .args(["daemon", "storage", "repair", "--apply", "--json", "--root"])
        .arg(root.path())
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();
    let report: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(report["registry_checkpoint"]["status"], "unrecoverable");
    assert_eq!(
        report["registry_checkpoint"]["files"][0]["state"],
        "unrecoverable"
    );
    assert!(report["registry_checkpoint"]["archive_path"].is_null());

    suite_cmd()
        .args(["daemon", "storage", "repair", "--apply", "--root"])
        .arg(root.path())
        .assert()
        .code(1)
        .stdout(predicate::str::contains("NO SAFE RECOVERY"))
        .stdout(predicate::str::contains(
            "content differs from the committed checkpoint",
        ));
    assert_eq!(fs::read(&task_path).unwrap(), edited);
    assert!(!root
        .path()
        .join(".packet28/daemon/registry-repair")
        .exists());
}

#[test]
fn storage_repair_completes_after_a_process_interruption() {
    let root = TempDir::new().unwrap();
    seed_task_registry(&root, &[("task", 0)]);
    let (task_path, committed) = whitespace_edit_task_registry(&root);

    suite_cmd()
        .args(["daemon", "storage", "repair", "--apply", "--root"])
        .arg(root.path())
        .env("PACKET28_REGISTRY_REPAIR_EXIT_AFTER", "journal")
        .assert()
        .code(87);
    assert!(root
        .path()
        .join(".packet28/daemon/.task-watch-checkpoint-v1.repair.json")
        .exists());
    assert!(
        packet28_daemon_core::storage::load_task_registry(root.path())
            .unwrap_err()
            .to_string()
            .contains("interrupted")
    );

    let pending = repair_json(&root, &[]);
    assert_eq!(
        pending["registry_checkpoint"]["status"],
        "interrupted_repair"
    );
    let resumed = repair_json(&root, &["--apply"]);
    assert_eq!(resumed["registry_checkpoint"]["status"], "resumed");
    assert_eq!(fs::read(&task_path).unwrap(), committed);
    assert!(
        packet28_daemon_core::storage::load_task_registry(root.path())
            .unwrap()
            .tasks
            .contains_key("task")
    );
}

#[test]
fn storage_repair_resume_refuses_a_missing_archived_original() {
    let root = TempDir::new().unwrap();
    seed_task_registry(&root, &[("task", 0), ("neighbor", 0)]);
    let (task_path, committed) = whitespace_edit_task_registry(&root);
    let edited = fs::read(&task_path).unwrap();
    let daemon = root.path().join(".packet28/daemon");
    let journal_path = daemon.join(".task-watch-checkpoint-v1.repair.json");

    suite_cmd()
        .args(["daemon", "storage", "repair", "--apply", "--root"])
        .arg(root.path())
        .env("PACKET28_REGISTRY_REPAIR_EXIT_AFTER", "journal")
        .assert()
        .code(87);
    let journal: Value = serde_json::from_slice(&fs::read(&journal_path).unwrap()).unwrap();
    let backup = daemon
        .join("registry-repair")
        .join(journal["archive"].as_str().unwrap())
        .join("original-task-registry-v1.json");
    assert_eq!(fs::read(&backup).unwrap(), edited);
    fs::remove_file(&backup).unwrap();
    let journal_bytes = fs::read(&journal_path).unwrap();

    for extra in [&[][..], &["--apply"][..]] {
        suite_cmd()
            .args(["daemon", "storage", "repair", "--json", "--root"])
            .arg(root.path())
            .args(extra)
            .assert()
            .failure()
            .stderr(predicate::str::contains(
                "the archived original original-task-registry-v1.json is missing or altered",
            ));
        assert_eq!(fs::read(&task_path).unwrap(), edited, "{extra:?}");
        assert_eq!(fs::read(&journal_path).unwrap(), journal_bytes, "{extra:?}");
        assert!(!backup.exists());
    }

    // Restoring the exact evidence lets the same journaled repair complete.
    fs::write(&backup, &edited).unwrap();
    let resumed = repair_json(&root, &["--apply"]);
    assert_eq!(resumed["registry_checkpoint"]["status"], "resumed");
    assert_eq!(fs::read(&task_path).unwrap(), committed);
    assert_eq!(fs::read(&backup).unwrap(), edited);
    assert!(!journal_path.exists());
}
