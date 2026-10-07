//! Persisted workspace-attestation compatibility, full-build bounds, and fail-closed metadata checks.
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

const MAX_INDEXED_FILE_BYTES: usize = 2 * 1024 * 1024;

fn fixture_head(root: &Path) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("read fixture HEAD");
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn write_marked(path: &Path, marker: &str, len: usize) {
    let mut bytes = format!("{marker}\n").into_bytes();
    bytes.resize(len, b'x');
    fs::write(path, bytes).unwrap();
}

type WriteWorkspace = Box<dyn Fn(&Path)>;

/// A stable dirty workspace that a query may not attest but a full build may.
struct BeyondQueryBound {
    label: &'static str,
    /// The query-time refusal for the same dirty set.
    query_refusal: &'static str,
    /// Every Git-dirty path the full build must attest.
    dirty: Vec<String>,
    /// Dirty paths attested but deliberately left out of the index.
    skipped: Vec<String>,
}

/// Lays out each case beyond one query bound (4,096 paths, 16 MiB, one
/// oversized file) but inside the full-build bounds, on a committed baseline.
///
/// The byte case reads every file into the budget: eight 2 MiB NUL-bearing
/// files (attested, never indexed) plus indexable text past 16 MiB. Indexing
/// 16 MiB of text instead takes minutes in a debug test build.
fn beyond_query_bound_cases() -> Vec<(BeyondQueryBound, WriteWorkspace)> {
    let paths = (0..4_097)
        .map(|i| format!("notes/p{i:04}.md"))
        .collect::<Vec<_>>();
    let binary = (0..8)
        .map(|i| format!("bulk/binary{i}.bin"))
        .collect::<Vec<_>>();
    let mut bulk = binary.clone();
    bulk.push("bulk/indexed.txt".to_string());
    let note_paths = paths.clone();
    let binary_paths = binary.clone();
    vec![
        (
            BeyondQueryBound {
                label: "4,097 untracked paths",
                query_refusal: "4096-path safety limit",
                dirty: paths,
                skipped: Vec::new(),
            },
            Box::new(move |root: &Path| {
                fs::create_dir_all(root.join("notes")).unwrap();
                for (i, path) in note_paths.iter().enumerate() {
                    fs::write(root.join(path), format!("note_marker_{i}\n")).unwrap();
                }
            }),
        ),
        (
            BeyondQueryBound {
                label: "16.25 MiB across files within the per-file limit",
                query_refusal: "bounded attestation byte limit",
                dirty: bulk,
                skipped: binary,
            },
            Box::new(move |root: &Path| {
                fs::create_dir_all(root.join("bulk")).unwrap();
                for path in &binary_paths {
                    write_marked(&root.join(path), "\0binary", MAX_INDEXED_FILE_BYTES);
                }
                write_marked(&root.join("bulk/indexed.txt"), "bulk_marker", 256 * 1024);
            }),
        ),
        (
            BeyondQueryBound {
                label: "oversized file beside dirty searchable source",
                query_refusal: "2097152-byte attestation limit",
                dirty: vec!["oversized.bin".to_string(), "src/lib.rs".to_string()],
                skipped: vec!["oversized.bin".to_string()],
            },
            Box::new(|root: &Path| {
                write_marked(
                    &root.join("oversized.bin"),
                    "oversized_hidden_marker",
                    MAX_INDEXED_FILE_BYTES + 1,
                );
                fs::write(
                    root.join("src/lib.rs"),
                    "pub fn dirty_searchable_marker() {}\n",
                )
                .unwrap();
            }),
        ),
    ]
}

fn discovered_paths(root: &Path) -> Vec<String> {
    fn walk(root: &Path, directory: &Path, paths: &mut Vec<String>) {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            let relative = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if relative == ".git" || relative == ".packet28" {
                continue;
            }
            if path.is_dir() {
                walk(root, &path, paths);
            } else {
                paths.push(relative);
            }
        }
    }
    let mut paths = Vec::new();
    walk(root, root, &mut paths);
    paths.sort();
    paths
}

/// Checks the published attestation, then that queries keep their own bounds.
fn assert_attested_beyond_query_bound(
    root: &Path,
    case: &BeyondQueryBound,
    runtime: &packet28_search_core::RegexIndexRuntime,
) {
    let label = case.label;
    let head = fixture_head(root);
    assert_eq!(
        runtime.manifest.workspace_attested_commit.as_deref(),
        Some(head.as_str()),
        "{label}"
    );
    assert_eq!(runtime.manifest.workspace_clean_commit, None, "{label}");
    let entries = current_record(root)["overlay_state"]["workspace_entries"].clone();
    let entries = entries.as_object().unwrap();
    let mut attested = entries.keys().cloned().collect::<Vec<_>>();
    let mut dirty = case.dirty.clone();
    attested.sort();
    dirty.sort();
    assert_eq!(attested, dirty, "{label}");
    for (path, digest) in entries {
        let digest = digest.as_str().unwrap();
        let bytes = fs::read(root.join(path)).unwrap();
        if bytes.len() > MAX_INDEXED_FILE_BYTES {
            assert_eq!(
                digest,
                format!("oversized:{}", bytes.len()),
                "{label} {path}"
            );
        } else {
            assert_eq!(
                digest,
                blake3::hash(&bytes).to_hex().as_str(),
                "{label} {path}"
            );
        }
    }
    let searchable = discovered_paths(root).len() - case.skipped.len();
    assert_eq!(runtime.manifest.indexed_files, searchable, "{label}");

    // A larger build budget does not widen query attestation: the published
    // generation exists, but queries refuse it rather than hash past their bounds.
    let reloaded = load_runtime(root).unwrap();
    assert!(!reloaded.is_loaded(), "{label}");
    let reason = reloaded.manifest.stale_reason.unwrap_or_default();
    assert!(reason.contains(case.query_refusal), "{label}: {reason}");
}

#[test]
fn full_rebuild_attests_stable_dirty_workspaces_beyond_query_bounds() {
    let mut rejected = Vec::new();
    for (case, write) in beyond_query_bound_cases() {
        let directory = tempdir().unwrap();
        let root = directory.path();
        commit_fixture(root, "pub fn committed_searchable_source() {}\n");
        write(root);

        match rebuild_full_index(root, true) {
            Ok(runtime) => assert_attested_beyond_query_bound(root, &case, &runtime),
            Err(error) => rejected.push(format!("{}: {error}", case.label)),
        }
    }
    assert!(rejected.is_empty(), "{rejected:#?}");
}

#[cfg(feature = "shared-repository-scan")]
#[test]
fn shared_scan_rebuild_attests_stable_dirty_workspaces_beyond_query_bounds() {
    use packet28_search_core::shared_scan::{wants_content, wants_path, RegexIndexScanSession};

    let mut rejected = Vec::new();
    for (case, write) in beyond_query_bound_cases() {
        let directory = tempdir().unwrap();
        let root = directory.path();
        commit_fixture(root, "pub fn committed_searchable_source() {}\n");
        write(root);
        let paths = discovered_paths(root);

        let prepared = RegexIndexScanSession::begin(root, true, &paths).and_then(|mut session| {
            for path in paths.iter().filter(|path| wants_path(path)) {
                let metadata = fs::metadata(root.join(path)).unwrap();
                let bytes = if wants_content(&metadata) {
                    fs::read(root.join(path)).unwrap()
                } else {
                    Vec::new()
                };
                session.ingest(path, &metadata, &bytes)?;
            }
            session.prepare()
        });
        let mut prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                rejected.push(format!("{}: {error}", case.label));
                continue;
            }
        };
        prepared.publish().unwrap();
        let runtime = prepared.commit().unwrap();

        let indexed = runtime.shared_scan_document_paths().unwrap();
        for skipped in &case.skipped {
            assert!(!indexed.contains(skipped), "{}", case.label);
        }
        assert_eq!(
            indexed.len(),
            paths.len() - case.skipped.len(),
            "{}",
            case.label
        );
        assert_attested_beyond_query_bound(root, &case, &runtime);
        let standard = rebuild_full_index(root, true).unwrap();
        assert_eq!(
            runtime.shared_scan_content_digests(),
            standard.shared_scan_content_digests(),
            "{}",
            case.label
        );
    }
    assert!(rejected.is_empty(), "{rejected:#?}");
}

#[test]
fn full_rebuild_keeps_its_own_workspace_byte_bound() {
    let directory = tempdir().unwrap();
    let root = directory.path();
    commit_fixture(root, "pub fn committed() {}\n");
    // 257 sparse, individually indexable 2 MiB files exceed the 512 MiB build budget.
    fs::create_dir_all(root.join("sparse")).unwrap();
    for i in 0..257 {
        let file = fs::File::create(root.join(format!("sparse/{i:03}.bin"))).unwrap();
        file.set_len(MAX_INDEXED_FILE_BYTES as u64).unwrap();
    }

    let error = rebuild_full_index(root, true).expect_err("build attestation exceeded 512 MiB");

    assert!(
        error.to_string().contains("bounded attestation byte limit"),
        "{error}"
    );
    assert!(!regex_dir(root).join("manifest.json").exists());
}
