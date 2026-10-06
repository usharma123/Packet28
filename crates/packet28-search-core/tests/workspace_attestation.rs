//! Persisted workspace-attestation compatibility and fail-closed metadata checks.
#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use packet28_search_core::{load_runtime, rebuild_full_index};
use serde_json::Value;
use tempfile::tempdir;

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("run fixture Git command");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn commit_fixture(root: &Path, source: &str) {
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), source).unwrap();
    fs::write(root.join(".gitignore"), ".packet28/\n").unwrap();
    git(root, &["init", "--quiet"]);
    git(root, &["add", "."]);
    git(
        root,
        &[
            "-c",
            "user.name=Packet28 Test",
            "-c",
            "user.email=packet28@example.invalid",
            "commit",
            "--quiet",
            "--no-gpg-sign",
            "-m",
            "fixture",
        ],
    );
}

fn regex_dir(root: &Path) -> PathBuf {
    root.join(".packet28/index/regex-v1")
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn record_path(root: &Path) -> PathBuf {
    let generation = read_json(&regex_dir(root).join("manifest.json"))["generation"]
        .as_u64()
        .unwrap();
    regex_dir(root).join(format!("generation-{generation:020}.json"))
}

fn current_record(root: &Path) -> Value {
    read_json(&record_path(root))
}

/// Rewrites the current generation in the legacy unbound format (no
/// publication fingerprint), which loaders still accept, after `edit` changes
/// its manifest and overlay state.
fn rewrite_unbound(root: &Path, edit: impl FnOnce(&mut Value, &mut Value)) {
    let path = record_path(root);
    let mut record = read_json(&path);
    let mut manifest = record["manifest"].take();
    let mut overlay = record["overlay_state"].take();
    edit(&mut manifest, &mut overlay);
    manifest
        .as_object_mut()
        .unwrap()
        .remove("publication_fingerprint");
    record["manifest"] = manifest.clone();
    record["overlay_state"] = overlay;
    fs::remove_file(&path).unwrap();
    fs::write(&path, serde_json::to_vec_pretty(&record).unwrap()).unwrap();
    fs::write(
        regex_dir(root).join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
}

fn stale_reason_after(root: &Path, edit: impl FnOnce(&mut Value, &mut Value)) -> String {
    rewrite_unbound(root, edit);
    let runtime = load_runtime(root).unwrap();
    assert!(!runtime.is_loaded(), "{:?}", runtime.manifest);
    assert_eq!(runtime.manifest.status, "stale", "{:?}", runtime.manifest);
    runtime.manifest.stale_reason.unwrap()
}

#[test]
fn clean_workspace_attestation_keeps_the_legacy_manifest_contract() {
    let directory = tempdir().unwrap();
    let root = directory.path();
    commit_fixture(root, "pub fn clean_contract() {}\n");
    let runtime = rebuild_full_index(root, true).unwrap();

    let raw = fs::read_to_string(regex_dir(root).join("manifest.json")).unwrap();
    assert!(raw.contains("\"workspace_clean_commit\""), "{raw}");
    assert!(!raw.contains("workspace_attested_commit"), "{raw}");
    assert!(current_record(root)["overlay_state"]
        .get("workspace_entries")
        .is_none());
    assert_eq!(runtime.manifest.workspace_attested_commit, None);

    rewrite_unbound(root, |_, _| {});
    assert!(
        load_runtime(root).unwrap().is_loaded(),
        "an unbound legacy clean generation must remain verified"
    );

    let reason = stale_reason_after(root, |manifest, _| {
        manifest
            .as_object_mut()
            .unwrap()
            .remove("workspace_clean_commit");
    });
    assert!(reason.contains("could not be authenticated"), "{reason}");
}

#[test]
fn dirty_workspace_attestation_fails_closed_on_metadata_loss_or_conflict() {
    let directory = tempdir().unwrap();
    let root = directory.path();
    commit_fixture(root, "pub fn committed() {}\n");
    fs::write(root.join("src/lib.rs"), "pub fn dirty_contract() {}\n").unwrap();
    let runtime = rebuild_full_index(root, true).unwrap();
    let head = runtime.manifest.workspace_attested_commit.clone().unwrap();
    assert_eq!(runtime.manifest.workspace_clean_commit, None);
    let entries = current_record(root)["overlay_state"]["workspace_entries"].clone();
    assert_eq!(
        entries
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["src/lib.rs"]
    );

    rewrite_unbound(root, |_, overlay| {
        overlay.as_object_mut().unwrap().remove("workspace_entries");
    });
    assert_eq!(
        load_runtime(root).unwrap().manifest.status,
        "corrupt",
        "dropping attested entries must fail the overlay digest"
    );

    let reason = stale_reason_after(root, |manifest, _| {
        manifest
            .as_object_mut()
            .unwrap()
            .remove("overlay_state_digest");
    });
    assert!(reason.contains("Git working tree changed"), "{reason}");

    let reason = stale_reason_after(root, |manifest, overlay| {
        overlay["workspace_entries"] = entries.clone();
        manifest
            .as_object_mut()
            .unwrap()
            .remove("workspace_attested_commit");
    });
    assert!(reason.contains("could not be authenticated"), "{reason}");

    let reason = stale_reason_after(root, |manifest, _| {
        manifest["workspace_attested_commit"] = Value::from(head.clone());
        manifest["workspace_clean_commit"] = Value::from(head.clone());
    });
    assert!(reason.contains("both"), "{reason}");

    let reason = stale_reason_after(root, |manifest, _| {
        manifest
            .as_object_mut()
            .unwrap()
            .remove("workspace_clean_commit");
        manifest["workspace_attested_commit"] = Value::from("0".repeat(40));
    });
    assert!(reason.contains("does not match"), "{reason}");

    rewrite_unbound(root, |manifest, _| {
        manifest["workspace_attested_commit"] = Value::from(head.clone());
    });
    assert!(load_runtime(root).unwrap().is_loaded());
}
