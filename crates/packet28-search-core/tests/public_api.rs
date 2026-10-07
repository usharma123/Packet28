use std::error::Error as _;
use std::fs;
use std::path::Path;
#[cfg(unix)]
use std::process::Command;

use packet28_reducer_core::{SearchRequest, SearchResult};
use packet28_search_core::{
    broker_internal_guarded_indexed_search_batch, clear_index, guarded_fallback_reason,
    guarded_indexed_search, guarded_indexed_search_batch, indexed_search,
    load_and_guarded_indexed_search, load_and_indexed_search, load_runtime, rebuild_full_index,
    rebuild_full_index_with_progress, update_overlay_index,
    BrokerInternalGuardedIndexedSearchSession, RegexIndexManifest, RegexIndexRuntime, Result,
    SearchError,
};
use tempfile::tempdir;

#[cfg(unix)]
fn run_fixture_git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("run fixture Git command");
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.first().copied().unwrap_or("command"),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
fn initialize_clean_git_fixture(root: &Path) {
    fs::write(root.join(".gitignore"), ".packet28/\n").expect("Git ignore fixture");
    run_fixture_git(root, &["init", "--quiet"]);
    run_fixture_git(root, &["config", "user.name", "Packet28 Test"]);
    run_fixture_git(root, &["config", "user.email", "packet28@example.invalid"]);
    run_fixture_git(root, &["add", "."]);
    run_fixture_git(
        root,
        &["commit", "--quiet", "--no-gpg-sign", "-m", "fixture"],
    );
}

#[test]
fn public_result_exposes_the_typed_index_unavailable_variant() {
    let root = tempdir().expect("temporary repository");
    let runtime = RegexIndexRuntime::default();
    let request = SearchRequest {
        query: "packet".to_string(),
        ..SearchRequest::default()
    };

    let error = indexed_search(root.path(), &runtime, &request).unwrap_err();

    assert!(matches!(error, SearchError::IndexNotLoaded), "{error:?}");
}

#[test]
fn combined_guarded_search_preserves_the_planner_fallback_reason() {
    let root = tempdir().unwrap();
    fs::create_dir_all(root.path().join("src")).unwrap();
    fs::write(root.path().join("src/lib.rs"), "pub fn alpha() {}\n").unwrap();
    let runtime = rebuild_full_index(root.path(), true).unwrap();
    let request = SearchRequest {
        query: "a".to_string(),
        fixed_string: true,
        ..SearchRequest::default()
    };
    let expected = guarded_fallback_reason(root.path(), &runtime, &request)
        .unwrap()
        .expect("weak request should fall back");

    let error = guarded_indexed_search(root.path(), &runtime, &request).unwrap_err();

    assert!(matches!(
        error,
        SearchError::IndexNotReady { reason } if reason == expected
    ));
}

#[test]
fn broker_internal_attested_batch_is_available_for_the_trusted_integration() {
    let root = tempdir().unwrap();
    fs::create_dir_all(root.path().join("src")).unwrap();
    fs::write(root.path().join("src/lib.rs"), "pub fn alpha() {}\n").unwrap();
    let runtime = rebuild_full_index(root.path(), true).unwrap();
    let request = SearchRequest {
        query: "alpha".to_string(),
        fixed_string: true,
        ..SearchRequest::default()
    };

    let mut session = BrokerInternalGuardedIndexedSearchSession::new();
    let results = broker_internal_guarded_indexed_search_batch(
        root.path(),
        &runtime,
        &[request],
        &mut session,
    )
    .unwrap();

    assert_eq!(results.len(), 1);
}

#[cfg(unix)]
#[test]
fn empty_guarded_batch_still_attests_workspace_freshness() {
    let root = tempdir().unwrap();
    fs::create_dir_all(root.path().join("src")).unwrap();
    let source = root.path().join("src/lib.rs");
    fs::write(&source, "pub fn original() {}\n").unwrap();
    initialize_clean_git_fixture(root.path());
    let runtime = rebuild_full_index(root.path(), true).unwrap();
    fs::write(&source, "pub fn dirty() {}\n").unwrap();

    let error = guarded_indexed_search_batch(root.path(), &runtime, &[]).unwrap_err();

    assert!(matches!(error, SearchError::IndexNotReady { .. }));
}

#[test]
fn invalid_regex_preserves_the_parser_source() {
    let root = tempdir().expect("temporary repository");
    fs::create_dir_all(root.path().join("src")).expect("source directory");
    fs::write(root.path().join("src/lib.rs"), "pub fn packet() {}\n").expect("source fixture");
    let runtime = rebuild_full_index(root.path(), true).expect("index fixture");
    let request = SearchRequest {
        query: "(".to_string(),
        ..SearchRequest::default()
    };

    let error = indexed_search(root.path(), &runtime, &request).unwrap_err();
    let SearchError::InvalidRegexSyntax {
        source: typed_source,
        ..
    } = &error
    else {
        panic!("expected typed regex syntax failure, found {error:?}");
    };
    let chained_source = error.source().expect("regex parser source");

    assert_eq!(chained_source.to_string(), typed_source.to_string());
}

#[test]
fn contextual_filesystem_failure_keeps_the_io_source_chain() {
    let root = tempdir().expect("temporary repository");
    let index_parent = root.path().join(".packet28/index");
    fs::create_dir_all(&index_parent).expect("index parent");
    fs::write(index_parent.join("regex-v1"), b"not a directory").expect("blocking file");

    let error = clear_index(root.path()).unwrap_err();
    let SearchError::Context {
        source: typed_source,
        ..
    } = &error
    else {
        panic!("expected contextual I/O failure, found {error:?}");
    };
    let io_source = typed_source
        .source()
        .and_then(|source| source.downcast_ref::<std::io::Error>());

    assert!(
        matches!(typed_source.as_ref(), SearchError::Io { .. }) && io_source.is_some(),
        "error={error:?}"
    );
}

#[test]
fn public_result_alias_accepts_rebuild_results() {
    let root = tempdir().expect("temporary repository");
    let result: Result<RegexIndexRuntime> = rebuild_full_index(root.path(), true);

    assert!(result.is_ok(), "{result:?}");
}

#[test]
fn public_error_is_send_sync_and_static() {
    fn assert_error_contract<T: std::error::Error + Send + Sync + 'static>() {}

    assert_error_contract::<SearchError>();
}

#[test]
fn root_entrypoint_signatures_remain_source_compatible() {
    type ProgressRebuild = fn(&Path, bool, fn(usize, usize)) -> Result<RegexIndexRuntime>;
    type BatchSearch =
        fn(&Path, &RegexIndexRuntime, &[SearchRequest]) -> Result<Vec<Option<SearchResult>>>;

    let _: fn(&Path) -> Result<RegexIndexRuntime> = load_runtime;
    let _: fn(&Path, bool) -> Result<RegexIndexRuntime> = rebuild_full_index;
    let _: ProgressRebuild = rebuild_full_index_with_progress::<fn(usize, usize)>;
    let _: fn(&Path, Option<&RegexIndexRuntime>, &[String]) -> Result<RegexIndexRuntime> =
        update_overlay_index;
    let _: fn(&Path) -> Result<()> = clear_index;
    let _: fn(&Path, &RegexIndexRuntime, &SearchRequest) -> Result<Option<String>> =
        guarded_fallback_reason;
    let _: fn(&Path, &RegexIndexRuntime, &SearchRequest) -> Result<SearchResult> =
        guarded_indexed_search;
    let _: BatchSearch = guarded_indexed_search_batch;
    let _: fn(&Path, &RegexIndexRuntime, &SearchRequest) -> Result<SearchResult> = indexed_search;
    let _: fn(&Path, &SearchRequest) -> Result<SearchResult> = load_and_guarded_indexed_search;
    let _: fn(&Path, &SearchRequest) -> Result<SearchResult> = load_and_indexed_search;

    fn assert_runtime_contract<T: Clone + Default + Send + Sync + 'static>() {}
    assert_runtime_contract::<RegexIndexRuntime>();
}

#[test]
fn manifest_json_contract_round_trips_every_public_field() {
    let manifest = RegexIndexManifest {
        schema_version: 3,
        weight_table_version: 2,
        generation: 17,
        publication_fingerprint: Some("publication-digest".to_string()),
        include_tests: true,
        status: "ready".to_string(),
        total_files: 11,
        indexed_files: 8,
        overlay_files: 3,
        overlay_segments: 2,
        overlay_state_digest: Some("overlay-digest".to_string()),
        base_commit: Some("deadbeef".to_string()),
        workspace_clean_commit: Some("deadbeef".to_string()),
        workspace_attested_commit: Some("deadbeef".to_string()),
        stale_reason: Some("fixture-stale".to_string()),
        last_build_started_at_unix: Some(101),
        last_build_completed_at_unix: Some(102),
        last_error: Some("fixture-error".to_string()),
    };

    let value = serde_json::to_value(&manifest).expect("serialize public manifest");
    assert_eq!(
        value,
        serde_json::json!({
            "schema_version": 3,
            "weight_table_version": 2,
            "generation": 17,
            "publication_fingerprint": "publication-digest",
            "include_tests": true,
            "status": "ready",
            "total_files": 11,
            "indexed_files": 8,
            "overlay_files": 3,
            "overlay_segments": 2,
            "overlay_state_digest": "overlay-digest",
            "base_commit": "deadbeef",
            "workspace_clean_commit": "deadbeef",
            "workspace_attested_commit": "deadbeef",
            "stale_reason": "fixture-stale",
            "last_build_started_at_unix": 101,
            "last_build_completed_at_unix": 102,
            "last_error": "fixture-error"
        })
    );
    assert_eq!(
        serde_json::from_value::<RegexIndexManifest>(value).expect("deserialize public manifest"),
        manifest
    );
}

fn assert_public_search_parity(root: &Path, runtime: &RegexIndexRuntime, request: SearchRequest) {
    let indexed = indexed_search(root, runtime, &request).expect("indexed search");
    let reducer = packet28_reducer_core::search(root, &request).expect("reducer search");
    assert_eq!(indexed.match_count, reducer.match_count, "{request:?}");
    assert_eq!(indexed.paths, reducer.paths, "{request:?}");
    assert_eq!(indexed.regions, reducer.regions, "{request:?}");
}

#[test]
fn root_facade_preserves_lifecycle_and_search_parity() {
    let directory = tempdir().expect("temporary repository");
    let root = directory.path();
    fs::create_dir_all(root.join("src/nested")).expect("source directories");
    fs::write(
        root.join("src/lib.rs"),
        "pub fn Alpha_service() {}\npub fn beta_service() {}\n",
    )
    .expect("primary fixture");
    fs::write(root.join("src/nested/mod.rs"), "pub struct AlphaVariant;\n")
        .expect("nested fixture");

    let rebuilt = rebuild_full_index(root, true).expect("root rebuild");
    let loaded = load_runtime(root).expect("root load");
    assert!(loaded.is_loaded());
    assert!(rebuilt.shares_base_with(&rebuilt.clone()));

    for request in [
        SearchRequest {
            query: "Alpha".to_string(),
            fixed_string: true,
            ..SearchRequest::default()
        },
        SearchRequest {
            query: "alpha".to_string(),
            fixed_string: true,
            case_sensitive: Some(false),
            ..SearchRequest::default()
        },
        SearchRequest {
            query: "Alpha|beta".to_string(),
            ..SearchRequest::default()
        },
        SearchRequest {
            query: "Alpha_service".to_string(),
            fixed_string: true,
            whole_word: true,
            ..SearchRequest::default()
        },
        SearchRequest {
            query: "AlphaVariant".to_string(),
            fixed_string: true,
            requested_paths: vec!["src/nested".to_string()],
            ..SearchRequest::default()
        },
    ] {
        assert_public_search_parity(root, &loaded, request);
    }

    fs::write(
        root.join("src/lib.rs"),
        "pub fn Alpha_service() {}\npub fn Gamma_service() {}\n",
    )
    .expect("overlay fixture");
    let updated = update_overlay_index(root, Some(&loaded), &["src/lib.rs".to_string()])
        .expect("root overlay update");
    let reloaded = load_runtime(root).expect("root reload");
    assert_eq!(updated.manifest, reloaded.manifest);
    let gamma = indexed_search(
        root,
        &reloaded,
        &SearchRequest {
            query: "Gamma_service".to_string(),
            fixed_string: true,
            ..SearchRequest::default()
        },
    )
    .expect("search updated content");
    assert_eq!(gamma.match_count, 1);

    clear_index(root).expect("root clear");
    assert!(!load_runtime(root).expect("load cleared index").is_loaded());
}

#[cfg(feature = "shared-repository-scan")]
#[test]
fn shared_scan_public_surface_prepares_and_publishes_a_generation() {
    use packet28_search_core::shared_scan::{
        wants_content, wants_path, PreparedRegexIndexRuntime, RegexIndexContentDigests,
        RegexIndexScanSession, MAX_SHARED_SCAN_CONTENT_BYTES,
    };

    let _: fn(&str) -> bool = wants_path;
    let _: fn(&fs::Metadata) -> bool = wants_content;
    let _: fn(&Path, bool, &[String]) -> Result<RegexIndexScanSession> =
        RegexIndexScanSession::begin;
    let _: fn(RegexIndexScanSession) -> Result<PreparedRegexIndexRuntime> =
        RegexIndexScanSession::prepare;
    let _: Option<RegexIndexContentDigests> = None;
    assert_ne!(std::hint::black_box(MAX_SHARED_SCAN_CONTENT_BYTES), 0);

    let directory = tempdir().expect("temporary repository");
    let root = directory.path();
    fs::create_dir_all(root.join("src")).expect("source directory");
    let bytes = b"pub fn SharedScanLiteral() {}\n";
    let path = root.join("src/lib.rs");
    fs::write(&path, bytes).expect("shared scan fixture");
    let metadata = fs::metadata(&path).expect("shared scan metadata");
    let paths = vec!["src/lib.rs".to_string()];

    let mut session = RegexIndexScanSession::begin(root, true, &paths).expect("begin shared scan");
    assert_eq!(session.total_files(), 1);
    assert!(wants_path(&paths[0]) && wants_content(&metadata));
    session
        .ingest(&paths[0], &metadata, bytes)
        .expect("ingest borrowed bytes");
    let mut prepared = session.prepare().expect("prepare shared generation");
    assert_eq!(prepared.manifest().indexed_files, 1);
    let prepared_digests = prepared.content_digests().expect("prepared digests");
    prepared.publish().expect("publish shared generation");
    let runtime = prepared.commit().expect("commit shared generation");

    assert_eq!(
        runtime.shared_scan_content_digests(),
        Some(prepared_digests)
    );
    assert_eq!(
        runtime.shared_scan_document_paths(),
        Some(vec!["src/lib.rs".to_string()])
    );
    let result = indexed_search(
        root,
        &runtime,
        &SearchRequest {
            query: "SharedScanLiteral".to_string(),
            fixed_string: true,
            ..SearchRequest::default()
        },
    )
    .expect("search shared generation");
    assert_eq!(result.match_count, 1);
}

#[cfg(unix)]
#[test]
fn incremental_update_rejects_a_missing_path_beneath_a_symlink() {
    use std::os::unix::fs::symlink;

    let root = tempdir().unwrap();
    let outside = tempdir().unwrap();
    fs::create_dir_all(root.path().join("src")).unwrap();
    fs::write(root.path().join("src/lib.rs"), "pub fn indexed() {}\n").unwrap();
    symlink(outside.path(), root.path().join("alias")).unwrap();
    let runtime = rebuild_full_index(root.path(), true).unwrap();

    let error = update_overlay_index(
        root.path(),
        Some(&runtime),
        &["alias/missing.rs".to_string()],
    )
    .expect_err("missing path beneath a symlink was accepted");

    assert!(matches!(error, SearchError::InvalidChangedPath { .. }));
}

#[cfg(unix)]
#[test]
fn missing_requested_path_does_not_search_through_a_symlinked_directory() {
    use std::os::unix::fs::symlink;

    let root = tempdir().unwrap();
    let outside = tempdir().unwrap();
    fs::create_dir_all(root.path().join("src")).unwrap();
    fs::write(root.path().join("src/lib.rs"), "pub fn indexed() {}\n").unwrap();
    fs::write(outside.path().join("needle.rs"), "pub fn outside() {}\n").unwrap();
    symlink(outside.path(), root.path().join("alias")).unwrap();
    let runtime = rebuild_full_index(root.path(), true).unwrap();
    let request = SearchRequest {
        query: "indexed".to_string(),
        fixed_string: true,
        requested_paths: vec!["needle.rs".to_string()],
        ..SearchRequest::default()
    };

    let result = indexed_search(root.path(), &runtime, &request).unwrap();

    assert!(result.resolved_paths.is_empty());
    assert_eq!(result.match_count, 0);
    assert!(result.paths.is_empty());
}

#[cfg(unix)]
#[test]
fn absolute_requested_child_resolves_under_a_relative_repository_root() {
    use std::os::unix::fs::symlink;

    let root = tempdir().unwrap();
    fs::create_dir_all(root.path().join("src")).unwrap();
    let source = root.path().join("src/lib.rs");
    fs::write(&source, "pub fn CanonicalChildNeedle() {}\n").unwrap();
    let anchor = tempfile::Builder::new()
        .prefix(".packet28-relative-root-")
        .tempdir_in(".")
        .unwrap();
    let linked_root = anchor.path().join("root");
    symlink(root.path(), &linked_root).unwrap();
    let current = std::env::current_dir().unwrap();
    let relative_root = linked_root.strip_prefix(&current).unwrap_or(&linked_root);
    assert!(!relative_root.is_absolute());
    let runtime = rebuild_full_index(relative_root, true).unwrap();
    let request = SearchRequest {
        query: "CanonicalChildNeedle".to_string(),
        fixed_string: true,
        requested_paths: vec![fs::canonicalize(source)
            .unwrap()
            .to_string_lossy()
            .into_owned()],
        ..SearchRequest::default()
    };

    let result = indexed_search(relative_root, &runtime, &request).unwrap();

    assert_eq!(result.resolved_paths, ["src/lib.rs"]);
    assert_eq!(result.match_count, 1);
}

#[cfg(target_os = "linux")]
#[test]
fn full_rebuild_skips_non_utf8_paths_that_alias_utf8_index_keys() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt as _;

    let directory = tempdir().unwrap();
    let root = directory.path();
    fs::create_dir_all(root.join("src")).unwrap();
    let invalid_name = OsString::from_vec(b"collision_\xff.rs".to_vec());
    fs::write(root.join("src").join(invalid_name), b"raw marker\n").unwrap();
    fs::write(root.join("src/collision_\u{fffd}.rs"), b"valid\n").unwrap();

    let runtime = rebuild_full_index(root, true).unwrap();
    let result = indexed_search(
        root,
        &runtime,
        &SearchRequest {
            query: "raw marker".to_string(),
            fixed_string: true,
            ..SearchRequest::default()
        },
    )
    .unwrap();
    assert_eq!(result.match_count, 0);
}

#[cfg(unix)]
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

#[cfg(unix)]
fn fixed(query: &str) -> SearchRequest {
    SearchRequest {
        query: query.to_string(),
        fixed_string: true,
        ..SearchRequest::default()
    }
}

#[cfg(unix)]
fn write_stable_dirty_workspace(root: &Path) {
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn committed_original() {}\n").unwrap();
    fs::write(root.join("src/removed.rs"), "pub fn removed_marker() {}\n").unwrap();
    fs::write(root.join("src/moved.rs"), "pub fn moved_marker_body() {}\n").unwrap();
    initialize_clean_git_fixture(root);
    fs::write(
        root.join("src/lib.rs"),
        "pub fn modified_file_attested_marker() {}\n",
    )
    .unwrap();
    fs::write(
        root.join("NOTES.md"),
        "new_file_attested_marker in user notes\n",
    )
    .unwrap();
    fs::remove_file(root.join("src/removed.rs")).unwrap();
    run_fixture_git(root, &["mv", "src/moved.rs", "src/renamed.rs"]);
}

#[cfg(unix)]
#[test]
fn full_rebuild_attests_a_stable_dirty_git_workspace_and_rejects_later_unreported_edits() {
    let directory = tempdir().unwrap();
    let root = directory.path();
    write_stable_dirty_workspace(root);
    let head = fixture_head(root);

    let runtime = rebuild_full_index(root, true).expect("stable dirty workspace rebuild");

    assert_eq!(runtime.manifest.base_commit.as_deref(), Some(head.as_str()));
    assert_eq!(
        runtime.manifest.workspace_attested_commit.as_deref(),
        Some(head.as_str())
    );
    assert_eq!(
        runtime.manifest.workspace_clean_commit, None,
        "a dirty build must never be labeled clean"
    );
    for (query, expected) in [
        ("modified_file_attested_marker", 1),
        ("new_file_attested_marker", 1),
        ("moved_marker_body", 1),
        ("committed_original", 0),
        ("removed_marker", 0),
    ] {
        let result = indexed_search(root, &runtime, &fixed(query)).unwrap();
        assert_eq!(result.match_count, expected, "{query}");
        if expected > 0 {
            guarded_indexed_search(root, &runtime, &fixed(query)).unwrap();
        }
    }

    let reloaded = load_runtime(root).unwrap();
    assert!(reloaded.is_loaded(), "{:?}", reloaded.manifest.stale_reason);
    assert_eq!(reloaded.manifest.generation, runtime.manifest.generation);
    let result = load_and_guarded_indexed_search(root, &fixed("modified_file_attested_marker"))
        .expect("reloaded dirty attestation");
    assert_eq!(result.match_count, 1);

    fs::write(root.join("src/lib.rs"), "pub fn unreported_edit() {}\n").unwrap();
    assert!(matches!(
        guarded_indexed_search(root, &runtime, &fixed("modified_file_attested_marker")),
        Err(SearchError::IndexNotReady { .. })
    ));
    let stale = load_runtime(root).unwrap();
    assert!(!stale.is_loaded(), "unreported edit reloaded as fresh");
    assert_eq!(stale.manifest.status, "stale");
    assert!(matches!(
        update_overlay_index(root, Some(&runtime), &["NOTES.md".to_string()]),
        Err(SearchError::IndexNotReady { .. })
    ));

    let updated = update_overlay_index(root, Some(&runtime), &["src/lib.rs".to_string()])
        .expect("reported change on an attested generation");
    assert_eq!(
        updated.manifest.workspace_attested_commit.as_deref(),
        Some(head.as_str())
    );
    assert_eq!(updated.manifest.workspace_clean_commit, None);
    let result = guarded_indexed_search(root, &updated, &fixed("unreported_edit")).unwrap();
    assert_eq!(result.match_count, 1);
    assert!(load_runtime(root).unwrap().is_loaded());

    run_fixture_git(root, &["add", "-A"]);
    run_fixture_git(root, &["commit", "--quiet", "--no-gpg-sign", "-m", "move"]);
    assert!(
        !load_runtime(root).unwrap().is_loaded(),
        "a HEAD change kept the dirty attestation"
    );
}

#[cfg(unix)]
type WorkspaceMutation = Box<dyn Fn(&Path)>;

#[cfg(unix)]
#[test]
fn full_rebuild_rejects_a_dirty_workspace_that_changes_during_the_build_without_replacing_the_ready_generation(
) {
    let directory = tempdir().unwrap();
    let root = directory.path();
    let source = root.join("src/lib.rs");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(&source, "pub fn original() {}\n").unwrap();
    initialize_clean_git_fixture(root);
    let ready = rebuild_full_index(root, true).unwrap();
    assert_eq!(ready.manifest.workspace_attested_commit, None);
    fs::write(&source, "pub fn dirty() {}\n").unwrap();

    let mutations: [(&str, WorkspaceMutation); 4] = [
        (
            "dirty tracked edit",
            Box::new(|root: &Path| fs::write(root.join("src/lib.rs"), "pub fn x() {}\n").unwrap()),
        ),
        (
            "new untracked file",
            Box::new(|root: &Path| fs::write(root.join("late.rs"), "pub fn late() {}\n").unwrap()),
        ),
        (
            "dirty path reverted",
            Box::new(|root: &Path| run_fixture_git(root, &["checkout", "--", "src/lib.rs"])),
        ),
        (
            "HEAD change",
            Box::new(|root: &Path| {
                run_fixture_git(
                    root,
                    &[
                        "commit",
                        "--quiet",
                        "--no-gpg-sign",
                        "--allow-empty",
                        "-m",
                        "late",
                    ],
                )
            }),
        ),
    ];
    for (label, mutate) in mutations {
        fs::write(&source, "pub fn dirty() {}\n").unwrap();
        let _ = fs::remove_file(root.join("late.rs"));
        let error = rebuild_full_index_with_progress(root, true, |completed, _| {
            if completed == 0 {
                mutate(root);
            }
        })
        .expect_err(label);

        assert!(
            matches!(error, SearchError::IndexNotReady { .. }),
            "{label}"
        );
        assert_eq!(
            load_manifest_generation(root),
            ready.manifest.generation,
            "{label}"
        );
    }
}

#[cfg(unix)]
fn load_manifest_generation(root: &Path) -> u64 {
    let raw = fs::read(root.join(".packet28/index/regex-v1/manifest.json")).unwrap();
    serde_json::from_slice::<RegexIndexManifest>(&raw)
        .unwrap()
        .generation
}

#[cfg(unix)]
#[test]
fn full_rebuild_rejects_dirty_workspace_aba_bytes() {
    let directory = tempdir().unwrap();
    let root = directory.path();
    let source = root.join("src/lib.rs");
    let dirty = "pub fn dirty_attested_bytes() {}\n";
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(&source, "pub fn committed() {}\n").unwrap();
    initialize_clean_git_fixture(root);
    fs::write(&source, dirty).unwrap();
    let ready = rebuild_full_index(root, true).unwrap();

    let error = rebuild_full_index_with_progress(root, true, |completed, total| {
        if completed == 0 {
            fs::write(&source, "pub fn transient_dirty_bytes() {}\n").unwrap();
        } else if completed == total {
            fs::write(&source, dirty).unwrap();
        }
    })
    .expect_err("dirty-build ABA bytes unexpectedly published");

    assert!(matches!(error, SearchError::IndexNotReady { .. }));
    let retained = load_runtime(root).unwrap();
    assert!(retained.is_loaded());
    assert_eq!(retained.manifest.generation, ready.manifest.generation);
}

#[cfg(unix)]
#[test]
fn full_rebuild_keeps_git_index_flag_and_symlink_rejection_for_dirty_workspaces() {
    let directory = tempdir().unwrap();
    let root = directory.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn committed() {}\n").unwrap();
    fs::write(root.join("AGENTS.md"), "committed instructions\n").unwrap();
    initialize_clean_git_fixture(root);
    fs::write(root.join("src/lib.rs"), "pub fn dirty() {}\n").unwrap();
    run_fixture_git(root, &["update-index", "--skip-worktree", "AGENTS.md"]);
    fs::write(root.join("AGENTS.md"), "hidden instructions\n").unwrap();

    assert!(matches!(
        rebuild_full_index(root, true),
        Err(SearchError::IndexNotReady { .. })
    ));

    run_fixture_git(root, &["update-index", "--no-skip-worktree", "AGENTS.md"]);
    std::os::unix::fs::symlink("src/lib.rs", root.join("alias.rs")).unwrap();
    assert!(matches!(
        rebuild_full_index(root, true),
        Err(SearchError::IndexNotReady { .. })
    ));
}

#[cfg(unix)]
#[test]
fn full_rebuild_rejects_clean_workspace_aba_bytes() {
    let directory = tempdir().unwrap();
    let root = directory.path();
    let source = root.join("src/lib.rs");
    let original = "pub fn original_bytes() {}\n";
    let transient = "pub fn transient_bytes() {}\n";
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(&source, original).unwrap();
    initialize_clean_git_fixture(root);
    let ready = rebuild_full_index(root, true).unwrap();

    let error = rebuild_full_index_with_progress(root, true, |completed, total| {
        if completed == 0 {
            fs::write(&source, transient).unwrap();
        } else if completed == total {
            fs::write(&source, original).unwrap();
        }
    })
    .expect_err("clean-build ABA bytes unexpectedly published");

    assert!(matches!(error, SearchError::IndexNotReady { .. }));
    let retained = load_runtime(root).unwrap();
    assert!(retained.is_loaded());
    assert_eq!(retained.manifest.generation, ready.manifest.generation);
}

#[cfg(all(unix, feature = "shared-repository-scan"))]
#[test]
fn shared_rebuild_rejects_borrowed_bytes_restored_before_prepare() {
    use packet28_search_core::shared_scan::RegexIndexScanSession;

    let directory = tempdir().unwrap();
    let root = directory.path();
    let source = root.join("src/lib.rs");
    let original = b"pub fn original_shared_bytes() {}\n";
    let transient = b"pub fn transient_shared_bytes() {}\n";
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(&source, original).unwrap();
    initialize_clean_git_fixture(root);
    let paths = vec!["src/lib.rs".to_string()];
    let mut session = RegexIndexScanSession::begin(root, true, &paths).unwrap();
    fs::write(&source, transient).unwrap();
    let metadata = fs::metadata(&source).unwrap();
    session.ingest(&paths[0], &metadata, transient).unwrap();
    fs::write(&source, original).unwrap();

    let error = session
        .prepare()
        .err()
        .expect("restored borrowed bytes unexpectedly authenticated");

    assert!(matches!(error, SearchError::IndexNotReady { .. }));
}
