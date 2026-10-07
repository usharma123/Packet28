use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Child;
use std::process::Command;
use std::sync::Barrier;
use std::time::{Duration, Instant};

use super::*;
use crate::tests::support::{daemon_test_root, daemon_test_state, TestDaemonState};

struct IndexFixture {
    state: TestDaemonState,
    root: PathBuf,
}

impl IndexFixture {
    fn new(files: &[(&str, &str)]) -> Self {
        let state = daemon_test_state();
        let root = daemon_test_root(&state);
        for (path, contents) in files {
            let path = root.join(path);
            fs::create_dir_all(path.parent().expect("fixture parent")).expect("create parent");
            fs::write(path, contents).expect("write fixture");
        }
        {
            let mut guard = state.lock().expect("state");
            guard.interactive_index.manifest = default_index_manifest(&root);
            guard
                .interactive_index
                .manifest
                .status
                .transition_to(DaemonIndexState::Queued)
                .expect("queue rebuild");
        }
        perform_full_index_rebuild(&state, None, None).expect("full rebuild");
        Self { state, root }
    }

    fn generated(file_count: usize) -> Self {
        let state = daemon_test_state();
        let root = daemon_test_root(&state);
        for idx in 0..file_count {
            write_generated_source(&root, idx, 0);
        }
        {
            let mut guard = state.lock().expect("state");
            guard.interactive_index.manifest = default_index_manifest(&root);
            guard
                .interactive_index
                .manifest
                .status
                .transition_to(DaemonIndexState::Queued)
                .expect("queue rebuild");
        }
        perform_full_index_rebuild(&state, None, None).expect("full rebuild");
        Self { state, root }
    }

    fn git(files: &[(&str, &str)]) -> Self {
        let state = daemon_test_state();
        let root = daemon_test_root(&state);
        for (path, contents) in files {
            let path = root.join(path);
            fs::create_dir_all(path.parent().expect("fixture parent")).expect("create parent");
            fs::write(path, contents).expect("write fixture");
        }
        initialize_git_repository(&root);
        {
            let mut guard = state.lock().expect("state");
            guard.interactive_index.manifest = default_index_manifest(&root);
            guard
                .interactive_index
                .manifest
                .status
                .transition_to(DaemonIndexState::Queued)
                .expect("queue rebuild");
        }
        perform_full_index_rebuild(&state, None, None).expect("full rebuild");
        Self { state, root }
    }

    fn repo_runtime(&self) -> mapy_core::RepoIndexRuntime {
        self.state
            .lock()
            .expect("state")
            .interactive_index
            .repo_runtime
            .clone()
            .expect("repo runtime")
    }

    fn regex_runtime(&self) -> packet28_search_core::RegexIndexRuntime {
        self.state
            .lock()
            .expect("state")
            .interactive_index
            .regex_runtime
            .clone()
            .expect("regex runtime")
    }

    fn update(&self, paths: &[&str]) {
        let paths = paths
            .iter()
            .map(|path| (*path).to_string())
            .collect::<Vec<_>>();
        perform_incremental_index_update(&self.state, &paths, None, None)
            .expect("incremental update");
    }
}

fn initialize_git_repository(root: &Path) {
    fs::write(root.join(".gitignore"), ".packet28/\n").expect("write gitignore");
    let run = |args: &[&str]| {
        let status = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .status()
            .expect("run git");
        assert!(status.success(), "git command failed: {args:?}");
    };
    run(&["init", "--quiet"]);
    run(&["add", "."]);
    run(&[
        "-c",
        "user.name=Packet28 Tests",
        "-c",
        "user.email=packet28-tests@example.invalid",
        "commit",
        "--quiet",
        "-m",
        "fixture",
    ]);
}

impl Drop for IndexFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn accepted_index_shutdown_is_idempotent_after_worker_exit() {
    for queue_already_full in [false, true] {
        let (ingress, receiver) = IndexIngress::new();
        if queue_already_full {
            ingress
                .send(IndexCommand::RebuildFull)
                .expect("fill wake queue");
        }
        ingress
            .send(IndexCommand::Shutdown)
            .expect("queue shutdown");
        let batch = receiver.recv_debounced().expect("receive shutdown");
        assert!(batch.shutdown_epoch.is_some());
        drop(receiver);

        ingress
            .send(IndexCommand::Shutdown)
            .expect("repeat accepted shutdown");
        let error = ingress
            .send(IndexCommand::RebuildFull)
            .expect_err("shutdown worker accepted new work");
        assert!(error.to_string().contains("shutting down"));
    }
}

#[test]
fn index_shutdown_preserves_a_worker_dead_before_first_send() {
    let (ingress, receiver) = IndexIngress::new();
    drop(receiver);

    for _ in 0..2 {
        let error = ingress
            .send(IndexCommand::Shutdown)
            .expect_err("dead worker was treated as an accepted shutdown");
        assert!(error.to_string().contains("index worker is not running"));
        assert!(ingress.pending.lock().unwrap().shutdown_epoch.is_none());
    }
}

#[test]
fn index_shutdown_publishes_daemon_intent_before_batch_consumption() {
    let (ingress, receiver) = IndexIngress::new();
    let shutdown = crate::runtime::ShutdownSignal::new();
    let worker_shutdown = shutdown.clone();
    let worker = thread::spawn(move || {
        let batch = receiver.recv_debounced().expect("receive shutdown");
        assert!(batch.shutdown_epoch.is_some());
        assert!(
            worker_shutdown.is_requested(),
            "index exited before daemon shutdown intent"
        );
    });

    ingress
        .request_shutdown(|| {
            assert!(
                matches!(
                    ingress.pending.try_lock(),
                    Err(std::sync::TryLockError::WouldBlock)
                ),
                "shutdown batch can be consumed before daemon intent is published"
            );
            shutdown.request();
        })
        .expect("queue shutdown and publish daemon intent");
    worker.join().expect("join index worker");
    ingress
        .send(IndexCommand::Shutdown)
        .expect("supervisor repeats shutdown");
}

#[test]
fn bounded_index_ingress_promotes_a_path_flood_to_one_full_rebuild() {
    let (ingress, receiver) = IndexIngress::new();
    for index in 0..(MAX_PENDING_INDEX_PATHS + 10_000) {
        ingress
            .send(IndexCommand::ReindexPaths(vec![format!("src/{index}.rs")]))
            .expect("queue path");
    }

    let (pending_paths, full_rebuild) = ingress.pending_counts();
    assert!(full_rebuild);
    assert!(pending_paths <= MAX_PENDING_INDEX_PATHS);
    let batch = receiver.recv_debounced().expect("receive coalesced work");
    assert!(batch.full_rebuild_epoch.is_some());
    assert!(batch.paths.len() <= MAX_PENDING_INDEX_PATHS);
    assert!(batch.clear_epoch.is_none());
    assert!(batch.shutdown_epoch.is_none());

    ingress
        .send(IndexCommand::ReindexPaths(vec![
            "x".repeat(MAX_INDEX_PATH_BYTES + 1)
        ]))
        .expect("queue oversized path");
    assert_eq!(ingress.pending_counts(), (0, true));
}

#[test]
fn persisted_queued_or_building_manifest_is_reenqueued_on_startup() {
    for status in [DaemonIndexState::Queued, DaemonIndexState::Building] {
        let state = daemon_test_state();
        let root = daemon_test_root(&state);
        let (ingress, receiver) = IndexIngress::new();
        {
            let mut guard = state.lock().expect("state");
            guard.index_tx = ingress;
            guard.interactive_index.manifest = default_index_manifest(&root);
            guard.interactive_index.manifest.status = status;
            save_index_manifest_file(&root, &guard.interactive_index.manifest)
                .expect("persist interrupted state");
        }

        assert!(state
            .lock()
            .expect("state")
            .interactive_index
            .needs_rebuild());
        enqueue_full_index_rebuild(&state).expect("requeue interrupted build");
        let batch = receiver.recv_debounced().expect("receive startup rebuild");
        assert!(batch.full_rebuild_epoch.is_some());
        assert_eq!(
            load_index_manifest_file(&root).status,
            DaemonIndexState::Queued
        );
        fs::remove_dir_all(root).expect("remove fixture");
    }
}

#[test]
fn failed_startup_enqueue_keeps_a_recoverable_manifest() {
    let state = daemon_test_state();
    let root = daemon_test_root(&state);
    let (ingress, receiver) = IndexIngress::new();
    drop(receiver);
    {
        let mut guard = state.lock().expect("state");
        guard.index_tx = ingress;
        guard.interactive_index.manifest = default_index_manifest(&root);
        guard.interactive_index.manifest.status = DaemonIndexState::Building;
        save_index_manifest_file(&root, &guard.interactive_index.manifest)
            .expect("persist interrupted build");
    }

    let error = enqueue_full_index_rebuild(&state)
        .expect_err("disconnected startup worker unexpectedly accepted work");

    assert!(error.to_string().contains("not running"));
    let persisted = load_index_manifest_file(&root);
    assert_eq!(persisted.status, DaemonIndexState::Queued);
    assert!(load_index_runtime_files(&root, persisted).needs_rebuild());
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn externally_built_regex_generation_is_hydrated_before_daemon_readiness() {
    let state = daemon_test_state();
    let root = daemon_test_root(&state);
    fs::create_dir_all(root.join("src")).expect("create source directory");
    fs::write(
        root.join("src/lib.rs"),
        "pub struct ExternalRegexGeneration;\n",
    )
    .expect("write source");
    packet28_search_core::rebuild_full_index(&root, true).expect("build external regex index");

    let (ingress, receiver) = IndexIngress::new();
    {
        let mut guard = state.lock().expect("state");
        guard.interactive_index = load_index_runtime_files(&root, default_index_manifest(&root));
        assert!(guard.interactive_index.regex_is_current());
        assert!(!guard.interactive_index.repo_is_current());
        guard.index_tx = ingress;
    }

    enqueue_initial_index_work(&state).expect("hydrate daemon-owned indexes");

    let guard = state.lock().expect("state");
    assert_eq!(
        guard.interactive_index.manifest.status,
        DaemonIndexState::Ready
    );
    assert!(guard.interactive_index.repo_is_current());
    assert!(guard.interactive_index.regex_is_current());
    drop(guard);
    assert!(
        receiver
            .recv_debounced_timeout(Duration::from_millis(50))
            .expect("inspect startup queue")
            .is_none(),
        "startup published readiness before synchronously hydrating the external generation"
    );
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn index_ingress_preserves_clear_order_while_coalescing() {
    let (ingress, receiver) = IndexIngress::new();
    ingress.send(IndexCommand::Clear).expect("queue clear");
    ingress
        .send(IndexCommand::ReindexPaths(vec!["src/a.rs".to_string()]))
        .expect("queue path after clear");

    let batch = receiver.recv_debounced().expect("receive clear batch");
    assert!(batch.clear_epoch.is_some());
    assert!(batch.full_rebuild_epoch.is_none());
    assert_eq!(
        batch.paths.keys().cloned().collect::<Vec<_>>(),
        vec!["src/a.rs"]
    );

    ingress
        .send(IndexCommand::RebuildFull)
        .expect("queue rebuild");
    ingress
        .send(IndexCommand::Clear)
        .expect("queue later clear");
    let batch = receiver.recv_debounced().expect("receive reset batch");
    assert!(batch.clear_epoch.is_some());
    assert!(batch.full_rebuild_epoch.is_none());
    assert!(batch.paths.is_empty());

    ingress
        .send(IndexCommand::Clear)
        .expect("queue final clear");
    ingress
        .send(IndexCommand::Shutdown)
        .expect("queue shutdown");
    let batch = receiver
        .recv_debounced()
        .expect("receive shutdown clear batch");
    assert!(batch.clear_epoch.is_some());
    assert!(batch.shutdown_epoch.is_some());
    let error = ingress
        .send(IndexCommand::ReindexPaths(vec![
            "src/stranded.rs".to_string()
        ]))
        .expect_err("work was accepted after the shutdown batch was drained");
    assert!(error.to_string().contains("shutting down"));
}

#[test]
fn clear_ack_is_durable_and_clear_during_a_build_supersedes_publication() {
    let fixture = IndexFixture::new(&[("src/a.rs", "pub fn alpha() {}\n")]);
    let (ingress, receiver) = IndexIngress::new();
    fixture.state.lock().expect("state").index_tx = ingress;
    enqueue_full_index_rebuild(&fixture.state).expect("queue full rebuild");
    let full = receiver.recv_debounced().expect("receive full rebuild");
    let full_epoch = full.full_rebuild_epoch.expect("full epoch");

    perform_full_index_rebuild_with_hooks(
        &fixture.state,
        None,
        Some(full_epoch),
        IndexFollowUp::default(),
        || {
            let response = daemon_index_clear(fixture.state.clone()).expect("ack clear");
            assert!(response.cleared);
            assert!(
                index_clear_is_pending(&fixture.root),
                "clear ACK preceded its durable pending intent"
            );
            Ok(())
        },
        || Ok(()),
    )
    .expect("finish superseded full build");

    let guard = fixture.state.lock().expect("state");
    assert_eq!(
        guard.interactive_index.manifest.status,
        DaemonIndexState::Queued
    );
    drop(guard);
    let restarted =
        load_index_runtime_files(&fixture.root, load_index_manifest_file(&fixture.root));
    assert_eq!(restarted.manifest.status, DaemonIndexState::Queued);
    assert!(restarted.needs_rebuild());

    let clear = receiver.recv_debounced().expect("receive clear");
    assert!(clear.clear_epoch.is_some());
    assert_eq!(
        process_index_batch_with_recovery(&fixture.state, &clear, None).expect("complete clear"),
        IndexBatchStatus::Complete
    );
    assert!(index_clear_is_complete(&fixture.root));
}

#[test]
fn clear_immediately_before_commit_prevents_ready_publication() {
    let fixture = IndexFixture::new(&[("src/a.rs", "pub fn alpha() {}\n")]);
    let (ingress, receiver) = IndexIngress::new();
    fixture.state.lock().expect("state").index_tx = ingress;
    enqueue_full_index_rebuild(&fixture.state).expect("queue full rebuild");
    let full = receiver.recv_debounced().expect("receive full rebuild");
    let full_epoch = full.full_rebuild_epoch.expect("full epoch");

    perform_full_index_rebuild_with_hooks(
        &fixture.state,
        None,
        Some(full_epoch),
        IndexFollowUp::default(),
        || Ok(()),
        || enqueue_index_clear(&fixture.state),
    )
    .expect("finish superseded publication");

    assert_eq!(
        fixture
            .state
            .lock()
            .expect("state")
            .interactive_index
            .manifest
            .status,
        DaemonIndexState::Queued
    );
    assert!(index_clear_is_pending(&fixture.root));
    let clear = receiver.recv_debounced().expect("receive clear");
    process_index_batch_with_recovery(&fixture.state, &clear, None)
        .expect("process clear before restart");
    let restarted =
        load_index_runtime_files(&fixture.root, load_index_manifest_file(&fixture.root));
    assert_eq!(restarted.manifest.status, DaemonIndexState::Missing);
    assert!(index_clear_is_complete(&fixture.root));
}

/// Request admission used to record durable index intent under the retained
/// index directory lease while holding `DaemonState`, and the builder holds
/// that lease across progress checkpoints that take the state mutex. That
/// cycle held admission until its lock timeout rejected the request, which is
/// how `setup` lost the rebuild it requests right after starting the daemon.
/// Admission now persists intent under the clear-state lease without the
/// state mutex, so a checkpoint parked inside the builder cannot delay it.
#[test]
fn rebuild_admission_succeeds_while_a_builder_checkpoint_holds_the_index_lease() {
    let fixture = IndexFixture::new(&[("src/a.rs", "pub fn alpha() {}\n")]);
    let (ingress, receiver) = IndexIngress::new();
    fixture.state.lock().expect("state").index_tx = ingress;
    enqueue_full_index_rebuild(&fixture.state).expect("queue the initial rebuild");
    let initial = receiver
        .recv_debounced()
        .expect("receive the initial rebuild");
    let initial_epoch = initial.full_rebuild_epoch.expect("initial epoch");
    let checkpoint_reached = Arc::new(Barrier::new(2));
    let release_builder = Arc::new(Barrier::new(2));

    let builder = std::thread::spawn({
        let state = fixture.state.clone();
        let checkpoint_reached = checkpoint_reached.clone();
        let release_builder = release_builder.clone();
        move || {
            let mut first_checkpoint = true;
            perform_full_index_rebuild_with_checkpoint_hook(
                &state,
                None,
                Some(initial_epoch),
                IndexFollowUp::default(),
                || Ok(()),
                || Ok(()),
                || {
                    if first_checkpoint {
                        first_checkpoint = false;
                        checkpoint_reached.wait();
                        release_builder.wait();
                    }
                },
            )
        }
    });
    checkpoint_reached.wait();
    // The builder holds the retained index lease inside its first checkpoint
    // and cannot reach the state mutex until it is released below.
    let started = Instant::now();
    enqueue_full_index_rebuild(&fixture.state)
        .expect("admit a rebuild while a builder checkpoint holds the index lease");
    let elapsed = started.elapsed();
    assert!(
        elapsed < crate::runtime_files::INDEX_CLEAR_LOCK_TIMEOUT,
        "admission waited {elapsed:?} on a lease-held checkpoint"
    );
    release_builder.wait();
    builder
        .join()
        .expect("join builder")
        .expect("finish the overlapped full build");
    assert_eq!(
        fixture
            .state
            .lock()
            .expect("state")
            .interactive_index
            .manifest
            .status,
        DaemonIndexState::Queued,
        "the admitted rebuild must supersede the overlapped publication"
    );

    let admitted = receiver
        .recv_debounced()
        .expect("receive the admitted rebuild");
    assert!(admitted.epoch > initial.epoch);
    assert_eq!(
        process_index_batch_with_recovery(&fixture.state, &admitted, None)
            .expect("complete the admitted rebuild"),
        IndexBatchStatus::Complete
    );
    let guard = fixture.state.lock().expect("state");
    assert_eq!(
        guard.interactive_index.manifest.status,
        DaemonIndexState::Ready
    );
    assert!(!guard.interactive_index.needs_rebuild());
    assert_eq!(
        guard.interactive_index.manifest.indexed_files,
        guard.interactive_index.manifest.total_files
    );
}

/// Admission must not depend on the generation lease at all. A builder that
/// holds the retained index lease for longer than the clear-state lock
/// timeout still admits full, clear, and incremental requests immediately,
/// their durable intent is readable meanwhile, and the queued order (rebuild,
/// clear, paths) supersedes the overlapped publication and then completes.
#[test]
fn admission_does_not_wait_for_a_long_build_holding_the_index_lease() {
    let fixture = IndexFixture::new(&[("src/a.rs", "pub fn alpha() {}\n")]);
    let (ingress, receiver) = IndexIngress::new();
    fixture.state.lock().expect("state").index_tx = ingress;
    enqueue_full_index_rebuild(&fixture.state).expect("queue the initial rebuild");
    let initial = receiver
        .recv_debounced()
        .expect("receive the initial rebuild");
    let initial_epoch = initial.full_rebuild_epoch.expect("initial epoch");
    let lease_held = Arc::new(Barrier::new(2));
    let release_builder = Arc::new(Barrier::new(2));

    let builder = std::thread::spawn({
        let state = fixture.state.clone();
        let lease_held = lease_held.clone();
        let release_builder = release_builder.clone();
        move || {
            let mut first_checkpoint = true;
            perform_full_index_rebuild_with_checkpoint_hook(
                &state,
                None,
                Some(initial_epoch),
                IndexFollowUp::default(),
                || Ok(()),
                || Ok(()),
                || {
                    if first_checkpoint {
                        first_checkpoint = false;
                        lease_held.wait();
                        release_builder.wait();
                    }
                },
            )
        }
    });
    lease_held.wait();
    let held_since = Instant::now();
    let timeout = crate::runtime_files::INDEX_CLEAR_LOCK_TIMEOUT;
    let admit = |label: &str, request: &dyn Fn() -> Result<()>| {
        let started = Instant::now();
        request().unwrap_or_else(|error| panic!("{label} was rejected: {error:#}"));
        let elapsed = started.elapsed();
        assert!(
            elapsed < timeout,
            "{label} waited {elapsed:?} on a held lease"
        );
    };
    let path = "src/a.rs".to_string();
    admit("full rebuild", &|| {
        enqueue_full_index_rebuild(&fixture.state)
    });
    admit("clear", &|| {
        daemon_index_clear(fixture.state.clone()).map(|_| ())
    });
    assert!(
        index_clear_is_pending(&fixture.root),
        "clear intent must be readable while the build holds the lease"
    );
    admit("incremental paths", &|| {
        enqueue_incremental_index_paths(&fixture.state, std::slice::from_ref(&path)).map(|_| ())
    });
    assert!(
        index_clear_requires_rebuild(&fixture.root),
        "paths admitted after the clear must record the rebuild durably"
    );
    // Keep the lease held past the admission deadline before releasing it.
    let minimum_hold = timeout + Duration::from_millis(250);
    if let Some(remaining) = minimum_hold.checked_sub(held_since.elapsed()) {
        std::thread::sleep(remaining);
    }
    release_builder.wait();
    builder
        .join()
        .expect("join builder")
        .expect("finish the overlapped full build");
    {
        let guard = fixture.state.lock().expect("state");
        assert_eq!(
            guard.interactive_index.manifest.status,
            DaemonIndexState::Queued,
            "the admitted clear must supersede the overlapped publication"
        );
        assert_eq!(
            guard.interactive_index.manifest.regex_status.as_deref(),
            Some("clear_pending")
        );
    }

    let mut processed = 0;
    while let Some(batch) = receiver
        .recv_debounced_timeout(Duration::from_secs(5))
        .expect("receive queued work")
    {
        assert_eq!(
            process_index_batch_with_recovery(&fixture.state, &batch, None)
                .expect("process queued work"),
            IndexBatchStatus::Complete
        );
        processed += 1;
        let guard = fixture.state.lock().expect("state");
        if guard.interactive_index.manifest.status == DaemonIndexState::Ready
            && guard.interactive_index.manifest.queued_paths.is_empty()
        {
            break;
        }
    }
    assert!(processed > 0, "no queued work was delivered");
    assert!(!index_clear_is_pending(&fixture.root));
    let guard = fixture.state.lock().expect("state");
    assert_eq!(
        guard.interactive_index.manifest.status,
        DaemonIndexState::Ready
    );
    assert!(guard.interactive_index.manifest.dirty_paths.is_empty());
    assert!(guard.interactive_index.manifest.queued_paths.is_empty());
    assert!(!guard.interactive_index.needs_rebuild());
}

#[test]
fn older_clear_completion_cannot_acknowledge_a_newer_durable_revision() {
    let fixture = IndexFixture::new(&[("src/a.rs", "pub fn alpha() {}\n")]);
    let (ingress, receiver) = IndexIngress::new();
    fixture.state.lock().expect("state").index_tx = ingress;
    enqueue_index_clear(&fixture.state).expect("queue first clear");
    let first = receiver.recv_debounced().expect("receive first clear");
    let first_epoch = first.clear_epoch.expect("first clear epoch");
    let first_revision = first.clear_revision.expect("first clear revision");

    perform_index_clear_with_hook(
        &fixture.state,
        Some(first_epoch),
        Some(first_revision),
        first.follow_up_after(first_epoch),
        || {
            assert_eq!(
                persist_index_clear_pending(&fixture.root)?,
                first_revision + 1
            );
            Ok(())
        },
    )
    .expect("finish older clear around a newer durable revision");

    assert!(index_clear_is_pending(&fixture.root));
    assert_eq!(
        fixture
            .state
            .lock()
            .expect("state")
            .interactive_index
            .manifest
            .status,
        DaemonIndexState::Queued
    );

    enqueue_persisted_index_clear(&fixture.state).expect("queue newer persisted clear");
    let newer = receiver.recv_debounced().expect("receive newer clear");
    assert!(newer.clear_revision > first.clear_revision);
    assert_eq!(
        process_index_batch_with_recovery(&fixture.state, &newer, None)
            .expect("complete newer clear"),
        IndexBatchStatus::Complete
    );
    assert!(index_clear_is_complete(&fixture.root));
}

#[test]
fn completed_clear_survives_restart_without_implicit_rebuild() {
    let fixture = IndexFixture::new(&[("src/a.rs", "pub fn alpha() {}\n")]);
    let (ingress, receiver) = IndexIngress::new();
    fixture.state.lock().expect("state").index_tx = ingress;
    daemon_index_clear(fixture.state.clone()).expect("queue clear");
    let clear = receiver.recv_debounced().expect("receive clear");
    process_index_batch_with_recovery(&fixture.state, &clear, None).expect("complete clear");
    assert!(index_clear_is_complete(&fixture.root));

    let restarted =
        load_index_runtime_files(&fixture.root, load_index_manifest_file(&fixture.root));
    assert_eq!(restarted.manifest.status, DaemonIndexState::Missing);
    assert!(restarted.repo_runtime.is_none());
    assert!(restarted.regex_runtime.is_none());
    let (restart_ingress, restart_receiver) = IndexIngress::new();
    {
        let mut guard = fixture.state.lock().expect("state");
        guard.interactive_index = restarted;
        guard.index_tx = restart_ingress;
    }
    enqueue_initial_index_work(&fixture.state).expect("evaluate restart work");
    assert!(
        restart_receiver
            .recv_debounced_timeout(Duration::from_millis(50))
            .expect("wait for startup work")
            .is_none(),
        "completed clear was implicitly rebuilt on restart"
    );
}

#[test]
fn explicit_rebuild_atomically_supersedes_a_completed_clear() {
    let fixture = IndexFixture::new(&[("src/a.rs", "pub fn alpha() {}\n")]);
    let (ingress, receiver) = IndexIngress::new();
    fixture.state.lock().expect("state").index_tx = ingress;
    enqueue_index_clear(&fixture.state).expect("queue clear");
    let clear = receiver.recv_debounced().expect("receive clear");
    process_index_batch_with_recovery(&fixture.state, &clear, None).expect("complete clear");
    assert!(index_clear_is_complete(&fixture.root));

    enqueue_full_index_rebuild(&fixture.state).expect("request explicit rebuild");
    assert!(!index_clear_is_complete(&fixture.root));
    assert!(!index_clear_is_pending(&fixture.root));

    let restarted =
        load_index_runtime_files(&fixture.root, load_index_manifest_file(&fixture.root));
    let (restart_ingress, restart_receiver) = IndexIngress::new();
    {
        let mut guard = fixture.state.lock().expect("state");
        guard.interactive_index = restarted;
        guard.index_tx = restart_ingress;
    }
    enqueue_initial_index_work(&fixture.state).expect("recover superseding rebuild");
    let rebuild = restart_receiver
        .recv_debounced()
        .expect("receive recovered rebuild");
    assert!(rebuild.full_rebuild_epoch.is_some());
    assert_eq!(
        process_index_batch_with_recovery(&fixture.state, &rebuild, None)
            .expect("complete explicit rebuild"),
        IndexBatchStatus::Complete
    );
    assert_eq!(
        fixture
            .state
            .lock()
            .expect("state")
            .interactive_index
            .manifest
            .status,
        DaemonIndexState::Ready
    );
}

#[test]
fn clear_completion_sync_failure_leaves_an_atomic_restart_state() {
    let state = daemon_test_state();
    let root = daemon_test_root(&state);
    persist_index_clear_pending(&root).expect("persist pending clear");

    let error = complete_index_clear_with_sync_for_test(&root, |_, _| {
        anyhow::bail!("injected directory sync failure")
    })
    .expect_err("injected sync failure unexpectedly succeeded");
    assert!(error
        .to_string()
        .contains("injected directory sync failure"));
    assert!(
        index_clear_is_complete(&root),
        "post-rename state was neither a complete nor retryable clear"
    );

    let restarted = load_index_runtime_files(&root, load_index_manifest_file(&root));
    assert_eq!(restarted.manifest.status, DaemonIndexState::Missing);
    let (ingress, receiver) = IndexIngress::new();
    {
        let mut guard = state.lock().expect("state");
        guard.interactive_index = restarted;
        guard.index_tx = ingress;
    }
    enqueue_initial_index_work(&state).expect("evaluate crash state");
    assert!(receiver
        .recv_debounced_timeout(Duration::from_millis(50))
        .expect("wait for crash recovery work")
        .is_none());
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn malformed_clear_state_never_decodes_as_complete_or_superseded() {
    let state = daemon_test_state();
    let root = daemon_test_root(&state);
    fs::create_dir_all(index_dir(&root)).expect("create index directory");
    for raw in [
        b"complete\n".as_slice(),
        b"complete 0\n",
        b"complete nope\n",
        b"complete 1 trailing\n",
        b"superseded\n",
        b"superseded 0\n",
        b"superseded 1 trailing\n",
        b"unknown 9\n",
    ] {
        fs::write(index_dir(&root).join("clear-state-v1"), raw)
            .expect("write malformed clear state");
        assert!(
            index_clear_is_pending(&root),
            "malformed state was not failed closed: {:?}",
            String::from_utf8_lossy(raw)
        );
        assert!(
            !index_clear_is_complete(&root),
            "malformed state decoded as complete: {:?}",
            String::from_utf8_lossy(raw)
        );
    }
    fs::write(index_dir(&root).join("clear-state-v1"), b"complete 7\n")
        .expect("write valid complete state");
    assert!(index_clear_is_complete(&root));
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn queued_paths_alone_prevent_a_ready_index_status() {
    let fixture = IndexFixture::new(&[("src/a.rs", "pub fn alpha() {}\n")]);
    let status = {
        let mut guard = fixture.state.lock().expect("state");
        guard.interactive_index.manifest.dirty_paths.clear();
        guard.interactive_index.manifest.queued_paths = vec!["src/a.rs".to_string()];
        build_index_status(&guard.interactive_index)
    };

    assert!(!status.ready);
    assert!(status.fallback_mode);
    assert_eq!(status.dirty_file_count, 0);
    assert_eq!(status.queued_file_count, 1);
}

#[test]
fn incremental_manifest_queue_is_hard_bounded() {
    let state = daemon_test_state();
    let paths = (0..=MAX_PENDING_INDEX_PATHS)
        .map(|index| format!("src/{index}.rs"))
        .collect::<Vec<_>>();

    let outcome = enqueue_incremental_index_paths(&state, &paths).expect("queue index flood");

    assert!(outcome.full);
    assert!(outcome.queued_paths.is_empty());
    let guard = state.lock().expect("state");
    assert_eq!(
        guard.interactive_index.manifest.status,
        DaemonIndexState::Queued
    );
    assert!(guard.interactive_index.manifest.dirty_paths.is_empty());
    assert!(guard.interactive_index.manifest.queued_paths.is_empty());
    drop(guard);

    let oversized_state = daemon_test_state();
    let oversized = vec!["x".repeat(MAX_INDEX_PATH_BYTES + 1)];
    let outcome = enqueue_incremental_index_paths(&oversized_state, &oversized)
        .expect("promote oversized path");
    assert!(outcome.full);
    assert!(outcome.queued_paths.is_empty());

    let padded_state = daemon_test_state();
    let padded = vec![" ".repeat(MAX_INDEX_PATH_BYTES + 1)];
    let outcome = enqueue_incremental_index_paths(&padded_state, &padded)
        .expect("promote oversized whitespace-bearing path");
    assert!(outcome.full);
    assert!(outcome.queued_paths.is_empty());

    let duplicate_state = daemon_test_state();
    let duplicates = vec!["src/repeated.rs".to_string(); MAX_INDEX_PATH_INPUTS + 1];
    let outcome = enqueue_incremental_index_paths(&duplicate_state, &duplicates)
        .expect("promote an excessive duplicate-input flood");
    assert!(outcome.full);
    assert!(outcome.queued_paths.is_empty());
}

#[test]
fn dirty_paths_are_canonical_root_relative_and_deleted_paths_do_not_need_to_exist() {
    let fixture = IndexFixture::new(&[("src/a.rs", "pub fn alpha() {}\n")]);
    let absolute = fixture.root.join("src/a.rs").to_string_lossy().to_string();
    let deleted_absolute = fixture
        .root
        .join("src/deleted.rs")
        .to_string_lossy()
        .to_string();
    let (normalized, requires_full, includes_root) = normalize_index_paths(
        &fixture.root,
        &[
            "./src/a.rs".to_string(),
            "src/nested/../a.rs".to_string(),
            absolute,
            "src/missing/../deleted.rs".to_string(),
            deleted_absolute,
            String::new(),
        ],
    )
    .expect("normalize in-root aliases");

    assert!(!requires_full);
    assert!(!includes_root);
    assert_eq!(
        normalized,
        vec!["src/a.rs".to_string(), "src/deleted.rs".to_string()]
    );
    assert!(
        normalize_index_paths(&fixture.root, &[" src/a.rs ".to_string()]).is_err(),
        "whitespace-bearing aliases must not silently target a different path"
    );
    #[cfg(unix)]
    assert!(
        normalize_index_paths(&fixture.root, &[r"src\a.rs".to_string()]).is_err(),
        "literal Unix backslashes must not silently target a slash path"
    );
    assert!(normalize_index_paths(&fixture.root, &["../escape.rs".to_string()]).is_err());
    let outside = fixture
        .root
        .parent()
        .expect("fixture parent")
        .join("outside-index-path.rs")
        .to_string_lossy()
        .to_string();
    assert!(normalize_index_paths(&fixture.root, &[outside]).is_err());

    #[cfg(unix)]
    {
        let outside_dir = fixture.root.with_extension("outside");
        fs::create_dir_all(outside_dir.join("nested")).expect("create outside dir");
        std::os::unix::fs::symlink(&outside_dir, fixture.root.join("src/outside-link"))
            .expect("create escaping symlink");
        let result =
            normalize_index_paths(&fixture.root, &["src/outside-link/secret.rs".to_string()]);
        assert!(result.is_err(), "symlink escape was accepted: {result:?}");

        std::os::unix::fs::symlink(
            outside_dir.join("nested"),
            fixture.root.join("src/outside-parent-link"),
        )
        .expect("create parent-traversal symlink");
        let result = normalize_index_paths(
            &fixture.root,
            &["src/outside-parent-link/../secret.rs".to_string()],
        );
        assert!(
            result.is_err(),
            "symlink escape hidden by parent traversal was accepted: {result:?}"
        );

        #[cfg(target_os = "linux")]
        {
            use std::os::unix::ffi::OsStringExt;
            let non_utf_directory =
                std::ffi::OsString::from_vec(vec![b'n', b'o', b'n', b'-', 0xff, b'u', b't', b'f']);
            fs::create_dir_all(fixture.root.join(&non_utf_directory))
                .expect("create non-UTF-8 directory");
            std::os::unix::fs::symlink(
                fixture.root.join(&non_utf_directory),
                fixture.root.join("src/non-utf-link"),
            )
            .expect("create non-UTF-8 in-root symlink");
            let result =
                normalize_index_paths(&fixture.root, &["src/non-utf-link/file.rs".to_string()]);
            assert!(
                result.is_err(),
                "non-UTF-8 canonical path was accepted through a lossy alias: {result:?}"
            );
        }
        fs::remove_dir_all(outside_dir).expect("remove outside dir");
    }
}

#[test]
fn clear_arriving_during_path_normalization_orders_the_rebuild_after_the_tombstone() {
    let fixture = IndexFixture::new(&[("src/a.rs", "pub fn alpha() {}\n")]);
    let (ingress, receiver) = IndexIngress::new();
    fixture.state.lock().expect("state").index_tx = ingress;

    let outcome = enqueue_incremental_index_paths_after_root_snapshot(
        &fixture.state,
        &["src/a.rs".to_string()],
        |_| enqueue_index_clear(&fixture.state),
    )
    .expect("normalize paths after queuing a concurrent clear");

    assert!(!outcome.full);
    assert_eq!(outcome.queued_paths, vec!["src/a.rs"]);
    assert!(
        index_clear_requires_rebuild(&fixture.root),
        "path work did not supersede the durable clear tombstone"
    );

    let batch = receiver
        .recv_debounced()
        .expect("receive clear and path work");
    let clear_epoch = batch.clear_epoch.expect("clear epoch");
    let path_epoch = *batch.paths.get("src/a.rs").expect("path epoch");
    assert!(
        clear_epoch < path_epoch,
        "path work was not ordered after the clear"
    );
    assert_eq!(
        process_index_batch_with_recovery(&fixture.state, &batch, None)
            .expect("clear and rebuild path"),
        IndexBatchStatus::Complete
    );
    assert!(!index_clear_is_pending(&fixture.root));
    assert!(!index_clear_is_complete(&fixture.root));
    assert_eq!(
        fixture
            .state
            .lock()
            .expect("state")
            .interactive_index
            .manifest
            .status,
        DaemonIndexState::Ready
    );
}

#[test]
fn persistent_retry_is_backed_off_and_an_explicit_command_interrupts_it() {
    let mut backoff = IndexRetryBackoff::default();
    assert_eq!(backoff.next_delay(), INDEX_RETRY_INITIAL_DELAY);
    assert_eq!(
        backoff.next_delay(),
        INDEX_RETRY_INITIAL_DELAY.saturating_mul(2)
    );
    backoff.reset();
    assert_eq!(backoff.next_delay(), INDEX_RETRY_INITIAL_DELAY);
    for _ in 0..64 {
        let _ = backoff.next_delay();
    }
    assert_eq!(backoff.next_delay(), INDEX_RETRY_MAX_DELAY);

    let state = daemon_test_state();
    let root = daemon_test_root(&state);
    let (ingress, receiver) = IndexIngress::new();
    state.lock().expect("state").index_tx = ingress.clone();
    let (attempt_tx, attempt_rx) = std::sync::mpsc::channel();
    let (delay_tx, delay_rx) = std::sync::mpsc::channel();
    let worker_state = state.clone();
    let worker = std::thread::spawn(move || {
        let mut attempts = 0;
        run_index_worker_with_processor_and_backoff(
            worker_state,
            receiver,
            IndexRetryBackoff::with_delays(Duration::from_secs(2), Duration::from_secs(8)),
            move |_, batch, _| {
                attempts += 1;
                attempt_tx.send(batch.clone()).expect("record attempt");
                if attempts < 4 {
                    Ok(IndexBatchStatus::Retry(batch.clone()))
                } else {
                    Ok(IndexBatchStatus::Complete)
                }
            },
            move |delay| delay_tx.send(delay).expect("record retry delay"),
        )
    });

    ingress
        .send(IndexCommand::RebuildFull)
        .expect("queue failing work");
    let first = attempt_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("receive first attempt");
    assert!(first.full_rebuild_epoch.is_some());
    assert!(
        attempt_rx.recv_timeout(Duration::from_millis(200)).is_err(),
        "persistent failure retried in a hot loop"
    );
    let first_delay = delay_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("record first retry delay");
    assert_eq!(first_delay, Duration::from_secs(2));

    let interrupted_at = Instant::now();
    ingress
        .send(IndexCommand::RebuildFull)
        .expect("interrupt retry with explicit command");
    let second = attempt_rx
        .recv_timeout(Duration::from_millis(500))
        .expect("explicit command did not interrupt retry delay");
    assert!(interrupted_at.elapsed() < Duration::from_millis(500));
    assert!(second.epoch > first.epoch);
    let second_delay = delay_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("record second retry delay");
    assert_eq!(second_delay, Duration::from_secs(4));
    assert!(
        second_delay > first_delay,
        "explicit ingress reset failure history to the initial retry delay"
    );
    ingress
        .send(IndexCommand::RebuildFull)
        .expect("interrupt second retry");
    let third = attempt_rx
        .recv_timeout(Duration::from_millis(500))
        .expect("second explicit command did not interrupt retry");
    assert!(third.epoch > second.epoch);
    assert_eq!(
        delay_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("record third retry delay"),
        Duration::from_secs(8)
    );
    ingress
        .send(IndexCommand::RebuildFull)
        .expect("interrupt third retry");
    let fourth = attempt_rx
        .recv_timeout(Duration::from_millis(500))
        .expect("third explicit command did not interrupt retry");
    assert!(fourth.epoch > third.epoch);
    ingress
        .send(IndexCommand::Shutdown)
        .expect("shut down worker");
    worker
        .join()
        .expect("join worker")
        .expect("worker shutdown");
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn concurrent_clear_state_writers_publish_unique_revisions_without_temp_collisions() {
    const WRITERS: usize = 16;
    let state = daemon_test_state();
    let root = daemon_test_root(&state);
    let barrier = Arc::new(Barrier::new(WRITERS));
    let revisions = std::thread::scope(|scope| {
        let handles = (0..WRITERS)
            .map(|_| {
                let root = root.clone();
                let barrier = barrier.clone();
                scope.spawn(move || {
                    barrier.wait();
                    persist_index_clear_pending(&root).expect("persist concurrent clear")
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("join clear writer"))
            .collect::<Vec<_>>()
    });
    let mut sorted = revisions;
    sorted.sort_unstable();
    assert_eq!(sorted, (1..=WRITERS as u64).collect::<Vec<_>>());
    assert!(index_clear_is_pending(&root));
    let temporary_count = fs::read_dir(index_dir(&root))
        .expect("read index dir")
        .filter_map(std::result::Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".clear-state-v1.")
        })
        .count();
    assert_eq!(temporary_count, 0);
    fs::remove_dir_all(root).expect("remove fixture");
}

#[cfg(unix)]
const CLEAR_STATE_PROCESS_MODE: &str = "PACKET28_TEST_CLEAR_STATE_PROCESS_MODE";
#[cfg(unix)]
const CLEAR_STATE_PROCESS_ROOT: &str = "PACKET28_TEST_CLEAR_STATE_PROCESS_ROOT";
#[cfg(unix)]
const CLEAR_STATE_PROCESS_OUTPUT: &str = "PACKET28_TEST_CLEAR_STATE_PROCESS_OUTPUT";

#[cfg(unix)]
#[test]
fn clear_state_process_helper() {
    let Ok(mode) = std::env::var(CLEAR_STATE_PROCESS_MODE) else {
        return;
    };
    let root =
        PathBuf::from(std::env::var_os(CLEAR_STATE_PROCESS_ROOT).expect("clear-state helper root"));
    let output = PathBuf::from(
        std::env::var_os(CLEAR_STATE_PROCESS_OUTPUT).expect("clear-state helper output"),
    );
    match mode.as_str() {
        "write" => {
            let revision =
                persist_index_clear_pending(&root).expect("persist cross-process clear state");
            fs::write(output, revision.to_string()).expect("write cross-process revision");
        }
        "hold" => {
            persist_index_clear_pending_with_parent_hook_for_test(&root, |_| {
                fs::write(&output, b"locked").context("publish held-lock readiness")?;
                loop {
                    std::thread::park();
                }
            })
            .expect("held-lock helper unexpectedly returned");
        }
        other => panic!("unknown clear-state helper mode: {other}"),
    }
}

#[cfg(unix)]
fn spawn_clear_state_process(root: &Path, mode: &str, output: &Path) -> Child {
    Command::new(std::env::current_exe().expect("resolve current test executable"))
        .arg("--exact")
        .arg("index::tests::clear_state_process_helper")
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env(CLEAR_STATE_PROCESS_MODE, mode)
        .env(CLEAR_STATE_PROCESS_ROOT, root)
        .env(CLEAR_STATE_PROCESS_OUTPUT, output)
        .spawn()
        .expect("spawn clear-state helper")
}

#[cfg(unix)]
#[test]
fn clear_state_lock_serializes_processes_and_is_released_after_a_crash() {
    const WRITERS: usize = 8;
    let state = daemon_test_state();
    let root = daemon_test_root(&state);
    let mut writers = (0..WRITERS)
        .map(|index| {
            let output = root.join(format!("clear-revision-{index}"));
            (
                output.clone(),
                spawn_clear_state_process(&root, "write", &output),
            )
        })
        .collect::<Vec<_>>();
    for (_, child) in &mut writers {
        assert!(child.wait().expect("wait for clear writer").success());
    }
    let mut revisions = writers
        .iter()
        .map(|(output, _)| {
            fs::read_to_string(output)
                .expect("read process revision")
                .parse::<u64>()
                .expect("parse process revision")
        })
        .collect::<Vec<_>>();
    revisions.sort_unstable();
    assert_eq!(revisions, (1..=WRITERS as u64).collect::<Vec<_>>());

    let ready = root.join("clear-lock-held");
    let mut holder = spawn_clear_state_process(&root, "hold", &ready);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        ready.exists(),
        "helper did not acquire the clear-state lock"
    );

    let blocked_at = Instant::now();
    let error = persist_index_clear_pending(&root)
        .expect_err("a held cross-process lock did not bound the caller");
    let blocked_for = blocked_at.elapsed();
    assert!(
        format!("{error:#}").contains("timed out"),
        "unexpected held-lock error: {error:#}"
    );
    assert!(
        blocked_for >= Duration::from_millis(500) && blocked_for < Duration::from_secs(2),
        "cross-process lock wait was not bounded near its configured deadline: {blocked_for:?}"
    );

    holder.kill().expect("crash held-lock helper");
    assert!(!holder.wait().expect("reap held-lock helper").success());

    let revision =
        persist_index_clear_pending(&root).expect("acquire clear lock after holder crash");
    assert_eq!(revision, WRITERS as u64 + 1);
    fs::remove_dir_all(root).expect("remove fixture");
}

#[cfg(unix)]
#[test]
fn precreated_clear_temp_symlink_cannot_write_outside_the_index_directory() {
    let state = daemon_test_state();
    let root = daemon_test_root(&state);
    fs::create_dir_all(index_dir(&root)).expect("create index dir");
    let outside = root.with_extension("outside-clear-state");
    fs::write(&outside, b"sentinel").expect("write outside sentinel");
    let nonce = u64::MAX;
    let temporary = index_clear_temporary_path_for_test(&root, nonce);
    std::os::unix::fs::symlink(&outside, &temporary).expect("precreate temp symlink");

    let error = persist_index_clear_pending_with_nonce_for_test(&root, nonce)
        .expect_err("precreated temp symlink was followed");

    assert!(error
        .to_string()
        .contains("create unique index clear state"));
    assert_eq!(
        fs::read(&outside).expect("read outside sentinel"),
        b"sentinel"
    );
    assert!(
        fs::symlink_metadata(&temporary)
            .expect("temp symlink remains")
            .file_type()
            .is_symlink(),
        "failed create removed an unowned temp entry"
    );
    fs::remove_file(outside).expect("remove outside sentinel");
    fs::remove_dir_all(root).expect("remove fixture");
}

#[cfg(unix)]
#[test]
fn clear_state_writer_rejects_a_symlinked_packet28_ancestor() {
    let state = daemon_test_state();
    let root = daemon_test_root(&state);
    let packet28 = root.join(".packet28");
    fs::remove_dir_all(&packet28).expect("remove real packet28 directory");
    let outside = root.with_extension("outside-packet28");
    fs::create_dir_all(&outside).expect("create outside packet28 target");
    std::os::unix::fs::symlink(&outside, &packet28).expect("symlink packet28 ancestor");

    let error =
        persist_index_clear_pending(&root).expect_err("symlinked packet28 ancestor was followed");

    assert!(
        !outside.join("index/clear-state-v1").exists(),
        "clear state escaped through a symlinked ancestor"
    );
    assert!(
        error
            .to_string()
            .contains("retain index clear state parent"),
        "unexpected ancestor rejection: {error:#}"
    );
    fs::remove_dir_all(root).expect("remove fixture");
    fs::remove_dir_all(outside).expect("remove outside target");
}

#[cfg(unix)]
#[test]
fn clear_state_writer_detects_parent_swap_without_writing_to_replacement() {
    let state = daemon_test_state();
    let root = daemon_test_root(&state);
    let outside = root.with_extension("outside-index-swap");
    fs::create_dir_all(&outside).expect("create replacement target");
    let retained = root.join(".packet28/index-retained");

    let error = persist_index_clear_pending_with_parent_hook_for_test(&root, |index| {
        fs::rename(index, &retained).context("move retained index directory")?;
        std::os::unix::fs::symlink(&outside, index).context("install replacement index symlink")?;
        Ok(())
    })
    .expect_err("parent swap was not detected");

    assert!(
        !outside.join("clear-state-v1").exists(),
        "clear state was published through the replacement parent"
    );
    assert!(
        error.to_string().contains("parent changed")
            || error.to_string().contains("Too many levels"),
        "unexpected parent-swap rejection: {error:#}"
    );
    fs::remove_dir_all(root).expect("remove fixture");
    fs::remove_dir_all(outside).expect("remove outside target");
}

#[test]
fn stale_clear_temp_from_an_old_process_does_not_block_publication() {
    let state = daemon_test_state();
    let root = daemon_test_root(&state);
    fs::create_dir_all(index_dir(&root)).expect("create index dir");
    let stale = index_clear_temporary_path_for_test(&root, u64::MAX - 2);
    fs::write(&stale, b"stale temp from a crashed process").expect("write stale temp");

    let revision = persist_index_clear_pending(&root).expect("publish around stale temp");

    assert_eq!(revision, 1);
    assert!(index_clear_is_pending(&root));
    assert_eq!(
        fs::read(&stale).expect("stale temp remains owned by old process"),
        b"stale temp from a crashed process"
    );
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn newly_created_clear_state_ancestors_are_restart_visible() {
    let state = daemon_test_state();
    let root = daemon_test_root(&state);
    fs::remove_dir_all(root.join(".packet28")).expect("remove packet28 ancestors");

    persist_index_clear_pending(&root).expect("publish clear through new ancestors");

    assert!(root.join(".packet28/index").is_dir());
    assert!(root.join(".packet28/index/clear-state-v1").is_file());
    assert!(
        index_clear_is_pending(&root),
        "newly created ancestor entries did not retain the clear state"
    );
    fs::remove_dir_all(root).expect("remove fixture");
}

#[cfg(unix)]
#[test]
fn failed_new_ancestor_sync_is_retried_even_when_the_directory_now_exists() {
    for failed_parent in ["root", "packet28"] {
        let state = daemon_test_state();
        let root = daemon_test_root(&state);
        fs::remove_dir_all(root.join(".packet28")).expect("remove packet28 ancestors");

        let error = open_index_clear_parent_with_sync_for_test(&root, |parent, directory| {
            if parent == failed_parent {
                Err(std::io::Error::other(format!(
                    "injected {parent} sync failure"
                )))
            } else {
                directory.sync_all()
            }
        })
        .expect_err("injected ancestor sync unexpectedly succeeded");
        assert!(
            error
                .to_string()
                .contains("retain index clear state parent"),
            "unexpected ancestor sync failure: {error:#}"
        );

        persist_index_clear_pending(&root).expect("retry ancestor durability");
        assert!(index_clear_is_pending(&root));
        assert!(root.join(".packet28/index/clear-state-v1").is_file());
        fs::remove_dir_all(root).expect("remove fixture");
    }
}

#[cfg(unix)]
#[test]
fn clear_transition_cannot_clobber_replacement_state_after_its_read() {
    let state = daemon_test_state();
    let root = daemon_test_root(&state);
    persist_index_clear_pending(&root).expect("persist original pending state");
    let packet28 = root.join(".packet28");
    let replacement = packet28.join("index-replacement");
    fs::create_dir_all(&replacement).expect("create replacement index");
    fs::write(replacement.join("clear-state-v1"), b"pending 99\n")
        .expect("write newer replacement state");
    let retained = packet28.join("index-transition-retained");

    let error = complete_index_clear_with_transition_hook_for_test(&root, |index| {
        fs::rename(index, &retained).context("move transition source")?;
        fs::rename(&replacement, index).context("install replacement state")?;
        Ok(())
    })
    .expect_err("transition published into a replacement directory");

    assert!(
        error.to_string().contains("parent changed"),
        "unexpected transition binding error: {error:#}"
    );
    assert_eq!(
        fs::read(packet28.join("index/clear-state-v1")).expect("read replacement state"),
        b"pending 99\n",
        "old complete transition clobbered the newer pending revision"
    );
    fs::remove_dir_all(root).expect("remove fixture");
}

#[cfg(unix)]
#[test]
fn clear_state_read_rejects_an_ancestor_swap_after_open() {
    let state = daemon_test_state();
    let root = daemon_test_root(&state);
    persist_index_clear_pending(&root).expect("persist pending clear");
    complete_index_clear(&root).expect("persist completed clear");
    let outside = root.with_extension("outside-read-swap");
    fs::create_dir_all(&outside).expect("create replacement directory");
    fs::write(outside.join("clear-state-v1"), b"complete 999\n").expect("write replacement state");
    let retained = root.join(".packet28/index-read-retained");

    let treated_as_pending = index_clear_is_pending_with_read_hook_for_test(&root, |index| {
        fs::rename(index, &retained).context("move opened index directory")?;
        std::os::unix::fs::symlink(&outside, index).context("install read replacement")?;
        Ok(())
    });

    assert!(
        treated_as_pending,
        "read accepted a clear state through a changed ancestor binding"
    );
    fs::remove_dir_all(root).expect("remove fixture");
    fs::remove_dir_all(outside).expect("remove outside target");
}

#[cfg(unix)]
#[test]
fn clear_state_read_revalidates_directory_after_file_identity_check() {
    let state = daemon_test_state();
    let root = daemon_test_root(&state);
    persist_index_clear_pending(&root).expect("persist pending clear");
    complete_index_clear(&root).expect("persist completed clear");
    let packet28 = root.join(".packet28");
    let replacement = packet28.join("index-read-replacement");
    fs::create_dir_all(&replacement).expect("create replacement directory");
    fs::write(replacement.join("clear-state-v1"), b"pending 99\n")
        .expect("write replacement state");
    let retained = packet28.join("index-read-final-retained");

    let treated_as_pending =
        index_clear_is_pending_with_final_binding_hook_for_test(&root, |index| {
            fs::rename(index, &retained)?;
            fs::rename(&replacement, index)?;
            Ok(())
        });

    assert!(
        treated_as_pending,
        "read accepted state from a directory detached after file identity validation"
    );
    assert_eq!(
        fs::read(packet28.join("index/clear-state-v1")).expect("read replacement state"),
        b"pending 99\n"
    );
    fs::remove_dir_all(root).expect("remove fixture");
}

#[cfg(unix)]
#[test]
fn clear_state_read_rejects_same_directory_replacement_after_identity_check() {
    let state = daemon_test_state();
    let root = daemon_test_root(&state);
    persist_index_clear_pending(&root).expect("persist pending clear");
    complete_index_clear(&root).expect("persist completed clear");

    let treated_as_pending =
        index_clear_is_pending_with_final_binding_hook_for_test(&root, |index| {
            let replacement = index.join(".clear-state-read-replacement");
            fs::write(&replacement, b"pending 99\n")?;
            fs::rename(replacement, index.join("clear-state-v1"))
        });

    assert!(
        treated_as_pending,
        "read returned stale complete after same-directory marker replacement"
    );
    assert_eq!(
        fs::read(index_dir(&root).join("clear-state-v1")).expect("read replacement state"),
        b"pending 99\n"
    );
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn newer_same_path_enqueue_survives_incremental_batch_completion() {
    let fixture = IndexFixture::new(&[("src/a.rs", "pub fn original() {}\n")]);
    let (ingress, receiver) = IndexIngress::new();
    fixture.state.lock().expect("state").index_tx = ingress;
    let path = "src/a.rs".to_string();

    fs::write(
        fixture.root.join(&path),
        "pub fn first_incremental_edit() {}\n",
    )
    .expect("write first edit");
    enqueue_incremental_index_paths(&fixture.state, std::slice::from_ref(&path))
        .expect("queue first edit");
    let first = receiver.recv_debounced().expect("receive first edit");

    perform_incremental_index_update_after_start(
        &fixture.state,
        &first.paths.keys().cloned().collect::<Vec<_>>(),
        None,
        Some(first.epoch),
        || {
            assert_eq!(
                fixture
                    .state
                    .lock()
                    .expect("state")
                    .interactive_index
                    .manifest
                    .status,
                DaemonIndexState::Building
            );
            fs::write(
                fixture.root.join(&path),
                "pub fn second_incremental_edit() {}\n",
            )
            .expect("write second edit");
            enqueue_incremental_index_paths(&fixture.state, std::slice::from_ref(&path)).map(|_| ())
        },
    )
    .expect("complete first edit");

    {
        let guard = fixture.state.lock().expect("state");
        assert_eq!(
            guard.interactive_index.manifest.status,
            DaemonIndexState::Ready
        );
        assert_eq!(
            guard.interactive_index.manifest.dirty_paths.as_slice(),
            std::slice::from_ref(&path)
        );
        assert_eq!(
            guard.interactive_index.manifest.queued_paths.as_slice(),
            std::slice::from_ref(&path)
        );
    }
    let restarted =
        load_index_runtime_files(&fixture.root, load_index_manifest_file(&fixture.root));
    assert!(restarted.needs_rebuild());

    let second = receiver.recv_debounced().expect("receive second edit");
    assert!(second.epoch > first.epoch);
    assert_eq!(
        process_index_batch_with_recovery(&fixture.state, &second, None)
            .expect("complete second edit"),
        IndexBatchStatus::Complete
    );
    let guard = fixture.state.lock().expect("state");
    assert!(guard.interactive_index.manifest.dirty_paths.is_empty());
    assert!(guard.interactive_index.manifest.queued_paths.is_empty());
    assert!(!guard.interactive_index.needs_rebuild());
}

#[test]
fn newer_path_enqueue_survives_full_batch_completion() {
    let fixture = IndexFixture::new(&[("src/a.rs", "pub fn original() {}\n")]);
    let (ingress, receiver) = IndexIngress::new();
    fixture.state.lock().expect("state").index_tx = ingress;
    let path = "src/a.rs".to_string();

    enqueue_full_index_rebuild(&fixture.state).expect("queue full rebuild");
    let full = receiver.recv_debounced().expect("receive full rebuild");
    perform_full_index_rebuild_after_start(&fixture.state, None, Some(full.epoch), || {
        assert_eq!(
            fixture
                .state
                .lock()
                .expect("state")
                .interactive_index
                .manifest
                .status,
            DaemonIndexState::Building
        );
        fs::write(
            fixture.root.join(&path),
            "pub fn edit_queued_during_full_build() {}\n",
        )
        .expect("write edit during full build");
        enqueue_incremental_index_paths(&fixture.state, std::slice::from_ref(&path)).map(|_| ())
    })
    .expect("complete full rebuild");

    {
        let guard = fixture.state.lock().expect("state");
        assert_eq!(
            guard.interactive_index.manifest.status,
            DaemonIndexState::Ready
        );
        assert_eq!(
            guard.interactive_index.manifest.dirty_paths.as_slice(),
            std::slice::from_ref(&path)
        );
        assert_eq!(
            guard.interactive_index.manifest.queued_paths.as_slice(),
            std::slice::from_ref(&path)
        );
    }
    let restarted =
        load_index_runtime_files(&fixture.root, load_index_manifest_file(&fixture.root));
    assert!(restarted.needs_rebuild());

    let incremental = receiver
        .recv_debounced()
        .expect("receive later incremental update");
    assert!(incremental.epoch > full.epoch);
    assert_eq!(
        process_index_batch_with_recovery(&fixture.state, &incremental, None)
            .expect("complete later incremental update"),
        IndexBatchStatus::Complete
    );
    let guard = fixture.state.lock().expect("state");
    assert!(guard.interactive_index.manifest.dirty_paths.is_empty());
    assert!(guard.interactive_index.manifest.queued_paths.is_empty());
    assert!(!guard.interactive_index.needs_rebuild());
}

#[test]
fn ready_manifest_with_dirty_paths_forces_restart_recovery() {
    let fixture = IndexFixture::new(&[("src/a.rs", "pub fn alpha() -> usize { 1 }\n")]);
    let (disconnected, receiver) = IndexIngress::new();
    drop(receiver);
    fixture.state.lock().expect("state").index_tx = disconnected;
    enqueue_incremental_index_paths(&fixture.state, &["src/a.rs".to_string()])
        .expect_err("disconnected index worker unexpectedly accepted work");

    let restarted =
        load_index_runtime_files(&fixture.root, load_index_manifest_file(&fixture.root));

    assert_eq!(restarted.manifest.status, DaemonIndexState::Ready);
    assert!(restarted.repo_is_current());
    assert!(restarted.regex_is_current());
    assert!(
        restarted.needs_rebuild(),
        "Ready+dirty restart incorrectly treated persisted work as complete"
    );
}

#[test]
fn queued_dirty_path_uses_live_search_and_forced_index_refuses_stale_results() {
    let fixture = IndexFixture::new(&[("src/a.rs", "pub fn before_queue() {}\n")]);
    fs::write(
        fixture.root.join("src/a.rs"),
        "pub fn newly_visible_queued_match() {}\n",
    )
    .expect("update queued source");
    let (ingress, _receiver) = IndexIngress::new();
    fixture.state.lock().expect("state").index_tx = ingress;
    enqueue_incremental_index_paths(&fixture.state, &["src/a.rs".to_string()])
        .expect("queue dirty path without running worker");
    let request = packet28_reducer_core::SearchRequest {
        query: "newly_visible_queued_match".to_string(),
        requested_paths: vec!["src/a.rs".to_string()],
        fixed_string: true,
        ..packet28_reducer_core::SearchRequest::default()
    };

    let live = daemon_packet28_search(
        fixture.state.clone(),
        packet28_daemon_protocol::message::Packet28SearchRequest {
            request: request.clone(),
            force_indexed: false,
        },
    )
    .expect("dirty search should use a live fallback");

    assert_eq!(live.match_count, 1);
    assert_eq!(live.paths, vec!["src/a.rs"]);
    assert!(
        live.engine
            .as_ref()
            .and_then(|engine| engine.fallback_reason.as_deref())
            .is_some_and(|reason| reason.contains("queued or dirty")),
        "live fallback did not preserve daemon dirty-path provenance: {:?}",
        live.engine
    );

    let mut alias_request = request;
    alias_request.requested_paths = vec!["src/nested/../a.rs".to_string()];
    let forced = daemon_packet28_search(
        fixture.state.clone(),
        packet28_daemon_protocol::message::Packet28SearchRequest {
            request: alias_request.clone(),
            force_indexed: true,
        },
    )
    .expect_err("forced indexed search returned a stale result");
    assert!(
        forced.downcast_ref::<DaemonIndexSearchNotReady>().is_some(),
        "forced dirty search did not return the typed not-ready error: {forced:#}"
    );

    let guard = daemon_packet28_search_guard(
        fixture.state.clone(),
        packet28_daemon_protocol::message::Packet28SearchRequest {
            request: alias_request,
            force_indexed: false,
        },
    )
    .expect("inspect dirty search guard");
    assert!(
        guard
            .fallback_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("queued or dirty")),
        "search guard did not observe queued daemon paths"
    );

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("a.rs", fixture.root.join("src/a-link.rs"))
            .expect("create in-root search alias");
        let symlink_guard = daemon_packet28_search_guard(
            fixture.state.clone(),
            packet28_daemon_protocol::message::Packet28SearchRequest {
                request: packet28_reducer_core::SearchRequest {
                    query: "newly_visible_queued_match".to_string(),
                    requested_paths: vec!["src/a-link.rs".to_string()],
                    fixed_string: true,
                    ..packet28_reducer_core::SearchRequest::default()
                },
                force_indexed: true,
            },
        )
        .expect("inspect symlink-alias guard");
        assert!(
            symlink_guard
                .fallback_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("queued or dirty")),
            "search guard missed a symlink alias for a dirty path"
        );
    }
}

#[test]
fn cached_daemon_runtime_detects_an_out_of_band_tracked_edit() {
    let fixture = IndexFixture::git(&[
        ("src/candidate.rs", "pub fn stable_candidate() {}\n"),
        ("src/noncandidate.rs", "pub fn stable_noncandidate() {}\n"),
    ]);
    fs::write(
        fixture.root.join("src/noncandidate.rs"),
        "pub fn daemon_out_of_band_workspace_needle() {}\n",
    )
    .expect("write out-of-band source");
    let request = packet28_reducer_core::SearchRequest {
        query: "daemon_out_of_band_workspace_needle".to_string(),
        fixed_string: true,
        ..packet28_reducer_core::SearchRequest::default()
    };

    let live = daemon_packet28_search(
        fixture.state.clone(),
        packet28_daemon_protocol::message::Packet28SearchRequest {
            request: request.clone(),
            force_indexed: false,
        },
    )
    .expect("out-of-band edit should use live search");
    assert_eq!(live.paths, vec!["src/noncandidate.rs"]);
    assert!(
        live.engine
            .as_ref()
            .and_then(|engine| engine.fallback_reason.as_deref())
            .is_some_and(|reason| reason.contains("workspace freshness")),
        "fallback omitted workspace freshness provenance: {:?}",
        live.engine
    );

    let forced = daemon_packet28_search(
        fixture.state.clone(),
        packet28_daemon_protocol::message::Packet28SearchRequest {
            request,
            force_indexed: true,
        },
    )
    .expect_err("forced indexed search accepted an out-of-band edit");
    assert!(forced.downcast_ref::<DaemonIndexSearchNotReady>().is_some());
}

#[test]
fn incremental_git_publication_serves_the_reported_tracked_edit() {
    let fixture = IndexFixture::git(&[(
        "src/candidate.rs",
        "pub fn before_incremental_update() {}\n",
    )]);
    fs::write(
        fixture.root.join("src/candidate.rs"),
        "pub fn authenticated_incremental_needle() {}\n",
    )
    .expect("write reported source");
    fixture.update(&["src/candidate.rs"]);
    let restarted =
        load_index_runtime_files(&fixture.root, load_index_manifest_file(&fixture.root));
    fixture.state.lock().expect("state").interactive_index = restarted;

    let result = daemon_packet28_search(
        fixture.state.clone(),
        packet28_daemon_protocol::message::Packet28SearchRequest {
            request: packet28_reducer_core::SearchRequest {
                query: "authenticated_incremental_needle".to_string(),
                fixed_string: true,
                ..packet28_reducer_core::SearchRequest::default()
            },
            force_indexed: true,
        },
    )
    .expect("forced indexed search should accept the attested incremental edit");

    assert_eq!(result.paths, vec!["src/candidate.rs"]);
}

#[test]
fn incremental_git_publication_rejects_a_later_unreported_edit() {
    let fixture = IndexFixture::git(&[
        ("src/candidate.rs", "pub fn original_candidate() {}\n"),
        ("src/unreported.rs", "pub fn original_unreported() {}\n"),
    ]);
    fs::write(
        fixture.root.join("src/candidate.rs"),
        "pub fn reported_incremental_needle() {}\n",
    )
    .expect("write reported source");
    fixture.update(&["src/candidate.rs"]);
    fs::write(
        fixture.root.join("src/unreported.rs"),
        "pub fn later_unreported_needle() {}\n",
    )
    .expect("write unreported source");

    let error = daemon_packet28_search(
        fixture.state.clone(),
        packet28_daemon_protocol::message::Packet28SearchRequest {
            request: packet28_reducer_core::SearchRequest {
                query: "later_unreported_needle".to_string(),
                fixed_string: true,
                ..packet28_reducer_core::SearchRequest::default()
            },
            force_indexed: true,
        },
    )
    .expect_err("forced indexed search accepted an unreported edit");

    assert!(error.downcast_ref::<DaemonIndexSearchNotReady>().is_some());
}

#[test]
fn git_index_flags_fail_workspace_attestation_closed() {
    for flag in ["--assume-unchanged", "--skip-worktree"] {
        let fixture = IndexFixture::git(&[(
            "src/candidate.rs",
            "pub fn original_flagged_candidate() {}\n",
        )]);
        let status = Command::new("git")
            .arg("-C")
            .arg(&fixture.root)
            .args(["update-index", flag, "src/candidate.rs"])
            .status()
            .expect("set Git index flag");
        assert!(status.success(), "failed to set {flag}");
        fs::write(
            fixture.root.join("src/candidate.rs"),
            "pub fn hidden_by_git_index_flag() {}\n",
        )
        .expect("write flagged source");

        let error = daemon_packet28_search(
            fixture.state.clone(),
            packet28_daemon_protocol::message::Packet28SearchRequest {
                request: packet28_reducer_core::SearchRequest {
                    query: "hidden_by_git_index_flag".to_string(),
                    fixed_string: true,
                    ..packet28_reducer_core::SearchRequest::default()
                },
                force_indexed: true,
            },
        )
        .expect_err("Git index flag bypassed workspace freshness");
        let not_ready = error
            .downcast_ref::<DaemonIndexSearchNotReady>()
            .expect("typed index-not-ready error");
        assert!(not_ready.reason.contains("Git index flag"));
    }
}

#[test]
fn oversized_dirty_file_fails_bounded_workspace_attestation_closed() {
    let fixture = IndexFixture::git(&[(
        "src/candidate.rs",
        "pub fn bounded_attestation_candidate() {}\n",
    )]);
    fs::write(
        fixture.root.join("oversized-untracked.bin"),
        vec![b'x'; 2 * 1024 * 1024 + 1],
    )
    .expect("write oversized dirty file");

    let error = daemon_packet28_search(
        fixture.state.clone(),
        packet28_daemon_protocol::message::Packet28SearchRequest {
            request: packet28_reducer_core::SearchRequest {
                query: "bounded_attestation_candidate".to_string(),
                fixed_string: true,
                ..packet28_reducer_core::SearchRequest::default()
            },
            force_indexed: true,
        },
    )
    .expect_err("oversized dirty file triggered unbounded workspace hashing");
    let not_ready = error
        .downcast_ref::<DaemonIndexSearchNotReady>()
        .expect("typed index-not-ready error");
    assert!(not_ready.reason.contains("attestation limit"));
}

#[test]
fn incremental_directory_report_is_not_a_file_attestation() {
    let fixture =
        IndexFixture::git(&[("src/candidate.rs", "pub fn before_directory_report() {}\n")]);
    fs::write(
        fixture.root.join("src/candidate.rs"),
        "pub fn changed_beneath_reported_directory() {}\n",
    )
    .expect("write nested change");

    let error = perform_incremental_index_update(&fixture.state, &["src".to_string()], None, None)
        .expect_err("directory report authenticated an unindexed descendant");

    assert!(error.to_string().contains("neither Git-dirty nor tracked"));
}

#[test]
fn restarted_daemon_rejects_an_index_after_an_out_of_band_rename() {
    let fixture = IndexFixture::git(&[(
        "src/original.rs",
        "pub fn daemon_restart_workspace_needle() {}\n",
    )]);
    fs::rename(
        fixture.root.join("src/original.rs"),
        fixture.root.join("src/renamed.rs"),
    )
    .expect("rename source while daemon is stopped");
    let restarted =
        load_index_runtime_files(&fixture.root, load_index_manifest_file(&fixture.root));
    fixture.state.lock().expect("state").interactive_index = restarted;
    let request = packet28_reducer_core::SearchRequest {
        query: "daemon_restart_workspace_needle".to_string(),
        fixed_string: true,
        ..packet28_reducer_core::SearchRequest::default()
    };

    let live = daemon_packet28_search(
        fixture.state.clone(),
        packet28_daemon_protocol::message::Packet28SearchRequest {
            request: request.clone(),
            force_indexed: false,
        },
    )
    .expect("restarted daemon should use live search");
    assert_eq!(live.paths, vec!["src/renamed.rs"]);

    let forced = daemon_packet28_search(
        fixture.state.clone(),
        packet28_daemon_protocol::message::Packet28SearchRequest {
            request,
            force_indexed: true,
        },
    )
    .expect_err("forced indexed search accepted the pre-rename generation");
    assert!(forced.downcast_ref::<DaemonIndexSearchNotReady>().is_some());
}

#[test]
fn ready_search_normalizes_requested_aliases_before_guard_and_execution() {
    let fixture = IndexFixture::new(&[
        ("src/a.rs", "pub fn scoped_alias_needle() -> usize { 1 }\n"),
        ("src/b.rs", "pub fn scoped_alias_needle() -> usize { 2 }\n"),
        ("src/c.rs", "pub fn unrelated_c() -> usize { 3 }\n"),
        ("src/d.rs", "pub fn unrelated_d() -> usize { 4 }\n"),
    ]);
    let search = |requested_path: &str| {
        daemon_packet28_search(
            fixture.state.clone(),
            packet28_daemon_protocol::message::Packet28SearchRequest {
                request: packet28_reducer_core::SearchRequest {
                    query: "scoped_alias_needle".to_string(),
                    requested_paths: vec![
                        requested_path.to_string(),
                        "src/c.rs".to_string(),
                        "src/d.rs".to_string(),
                    ],
                    fixed_string: true,
                    ..packet28_reducer_core::SearchRequest::default()
                },
                force_indexed: true,
            },
        )
        .expect("run forced indexed alias search")
    };

    let parent_alias = search("src/nested/../a.rs");
    assert_eq!(parent_alias.paths, vec!["src/a.rs"]);
    assert_eq!(parent_alias.returned_match_count, 1);

    let root_alias = search(".");
    assert_eq!(
        root_alias.paths,
        vec!["src/a.rs".to_string(), "src/b.rs".to_string()]
    );
    let absolute_root = fixture.root.to_string_lossy().to_string();
    let absolute_root_alias = search(&absolute_root);
    assert_eq!(
        absolute_root_alias.paths,
        vec!["src/a.rs".to_string(), "src/b.rs".to_string()]
    );

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("a.rs", fixture.root.join("src/a-link.rs"))
            .expect("create indexed-search symlink alias");
        let symlink_alias = search("src/a-link.rs");
        assert_eq!(symlink_alias.paths, vec!["src/a.rs"]);
        assert_eq!(symlink_alias.returned_match_count, 1);
    }
}

#[test]
fn dirty_path_overlap_check_is_bounded_at_maximum_cardinality() {
    let mut manifest = default_index_manifest(Path::new("/workspace"));
    manifest.status = DaemonIndexState::Ready;
    manifest.dirty_paths = (0..MAX_PENDING_INDEX_PATHS)
        .map(|index| format!("dirty/{index:04}/file.rs"))
        .collect();
    let request = packet28_reducer_core::SearchRequest {
        query: "needle".to_string(),
        requested_paths: (0..MAX_PENDING_INDEX_PATHS)
            .map(|index| format!("requested/{index:04}/file.rs"))
            .collect(),
        fixed_string: true,
        ..packet28_reducer_core::SearchRequest::default()
    };

    assert_eq!(
        daemon_manifest_search_fallback_reason(&manifest, &request),
        None
    );
}

#[test]
fn search_guard_reports_missing_stale_and_unsupported_states_even_when_forced() {
    let missing = daemon_test_state();
    let forced_missing = daemon_packet28_search_guard(
        missing.clone(),
        packet28_daemon_protocol::message::Packet28SearchRequest {
            request: packet28_reducer_core::SearchRequest {
                query: "alpha".to_string(),
                fixed_string: true,
                ..packet28_reducer_core::SearchRequest::default()
            },
            force_indexed: true,
        },
    )
    .expect("inspect missing guard");
    assert!(forced_missing
        .fallback_reason
        .as_deref()
        .is_some_and(|reason| reason.contains("Missing")));
    let fixture = IndexFixture::new(&[("src/a.rs", "pub fn alpha() {}\n")]);
    fixture
        .state
        .lock()
        .expect("state")
        .interactive_index
        .manifest
        .dirty_paths
        .push("src/a.rs".to_string());
    let forced_stale = daemon_packet28_search_guard(
        fixture.state.clone(),
        packet28_daemon_protocol::message::Packet28SearchRequest {
            request: packet28_reducer_core::SearchRequest {
                query: "alpha".to_string(),
                fixed_string: true,
                ..packet28_reducer_core::SearchRequest::default()
            },
            force_indexed: true,
        },
    )
    .expect("inspect stale guard");
    assert!(forced_stale
        .fallback_reason
        .as_deref()
        .is_some_and(|reason| reason.contains("queued or dirty")));

    fixture
        .state
        .lock()
        .expect("state")
        .interactive_index
        .manifest
        .dirty_paths
        .clear();
    let forced_unsupported = daemon_packet28_search_guard(
        fixture.state.clone(),
        packet28_daemon_protocol::message::Packet28SearchRequest {
            request: packet28_reducer_core::SearchRequest {
                query: "a".to_string(),
                fixed_string: true,
                ..packet28_reducer_core::SearchRequest::default()
            },
            force_indexed: true,
        },
    )
    .expect("inspect unsupported guard");
    assert!(
        forced_unsupported
            .fallback_reason
            .as_deref()
            .is_some_and(|reason| {
                reason.contains("planner")
                    || reason.contains("too short")
                    || reason.contains("broad")
            }),
        "forced guard suppressed unsupported-plan fallback: {forced_unsupported:?}"
    );
}

#[test]
fn partial_clear_reconciles_non_ready_and_restart_refuses_forced_search() {
    let fixture = IndexFixture::new(&[("src/a.rs", "pub fn alpha() {}\n")]);
    let (ingress, receiver) = IndexIngress::new();
    fixture.state.lock().expect("state").index_tx = ingress;
    enqueue_index_clear(&fixture.state).expect("queue clear");
    let clear = receiver.recv_debounced().expect("receive clear");
    let clear_epoch = clear.clear_epoch.expect("clear epoch");

    let failure = perform_index_clear_with_hook(
        &fixture.state,
        Some(clear_epoch),
        clear.clear_revision,
        clear.follow_up_after(clear_epoch),
        || anyhow::bail!("injected fault between index engines"),
    )
    .expect_err("partial clear unexpectedly completed");
    reconcile_failed_index_clear(&fixture.state, &failure).expect("reconcile partial clear");

    {
        let guard = fixture.state.lock().expect("state");
        assert_eq!(
            guard.interactive_index.manifest.status,
            DaemonIndexState::Queued
        );
        assert!(guard.interactive_index.repo_runtime.is_none());
        assert!(
            guard.interactive_index.regex_runtime.is_some(),
            "surviving regex generation was not reconciled"
        );
    }
    assert!(index_clear_is_pending(&fixture.root));
    let restarted =
        load_index_runtime_files(&fixture.root, load_index_manifest_file(&fixture.root));
    assert_eq!(restarted.manifest.status, DaemonIndexState::Queued);
    fixture.state.lock().expect("state").interactive_index = restarted;

    let forced = daemon_packet28_search(
        fixture.state.clone(),
        packet28_daemon_protocol::message::Packet28SearchRequest {
            request: packet28_reducer_core::SearchRequest {
                query: "alpha".to_string(),
                fixed_string: true,
                ..packet28_reducer_core::SearchRequest::default()
            },
            force_indexed: true,
        },
    )
    .expect_err("forced search read a partially cleared generation");
    assert!(forced.downcast_ref::<DaemonIndexSearchNotReady>().is_some());

    assert_eq!(
        process_index_batch_with_recovery(&fixture.state, &clear, None)
            .expect("retry partial clear"),
        IndexBatchStatus::Complete
    );
    assert!(index_clear_is_complete(&fixture.root));
}

#[cfg(unix)]
#[test]
fn retained_repository_clear_never_follows_a_replaced_index_parent() {
    let fixture = IndexFixture::new(&[("src/a.rs", "pub fn alpha() {}\n")]);
    let outside = tempfile::tempdir().expect("create outside directory");
    let outside_repo = outside.path().join("mapy-v1");
    fs::create_dir_all(&outside_repo).expect("create outside repository tree");
    let outside_sentinel = outside_repo.join("do-not-delete");
    fs::write(&outside_sentinel, b"outside").expect("write outside sentinel");
    let index = index_dir(&fixture.root);
    let retained = index.with_file_name("index-retained-during-clear");

    let failure = clear_index_files_with_binding_hook_for_test(&fixture.root, || {
        fs::rename(&index, &retained).context("rename retained index directory")?;
        std::os::unix::fs::symlink(outside.path(), &index)
            .context("replace index directory with outside symlink")
    })
    .expect_err("replaced index binding unexpectedly cleared successfully");

    assert!(
        failure.to_string().contains("binding changed"),
        "clear failed for an unrelated reason: {failure:#}"
    );
    assert!(
        outside_sentinel.exists(),
        "retained clear followed the replacement symlink outside Packet28 state"
    );
    assert!(
        !retained.join("mapy-v1").exists(),
        "retained repository tree was not cleared through its directory handle"
    );
    fs::remove_file(&index).expect("remove replacement symlink");
    fs::rename(&retained, &index).expect("restore retained index directory");
}

#[cfg(unix)]
#[test]
fn repository_clear_fence_never_writes_through_a_symlinked_index_parent() {
    let fixture = IndexFixture::new(&[("src/a.rs", "pub fn alpha() {}\n")]);
    let outside = tempfile::tempdir().expect("create outside directory");
    let outside_sentinel = outside.path().join("do-not-write");
    fs::write(&outside_sentinel, b"outside").expect("write outside sentinel");
    let index = index_dir(&fixture.root);
    let retained = index.with_file_name("index-retained-before-clear");
    fs::rename(&index, &retained).expect("retain real index directory");
    std::os::unix::fs::symlink(outside.path(), &index)
        .expect("replace index directory with outside symlink");

    let failure = clear_index_files(&fixture.root)
        .expect_err("symlinked index parent unexpectedly cleared successfully");

    assert!(
        failure
            .to_string()
            .contains("retain repository index parent"),
        "clear failed for an unrelated reason: {failure:#}"
    );
    assert_eq!(
        fs::read(&outside_sentinel).expect("outside sentinel"),
        b"outside"
    );
    assert!(
        !outside
            .path()
            .join(".mapy-v1.generation-high-water.json")
            .exists(),
        "generation fence wrote through the replacement symlink"
    );
    fs::remove_file(&index).expect("remove replacement symlink");
    fs::rename(&retained, &index).expect("restore retained index directory");
}

#[cfg(unix)]
#[test]
fn repository_clear_rejects_a_substituted_generation_fence_temporary_file() {
    let fixture = IndexFixture::new(&[("src/a.rs", "pub fn alpha() {}\n")]);
    let mapy = fixture.root.join(".packet28/index/mapy-v1");
    let manifest_path = mapy.join("manifest.json");
    let manifest_before = fs::read(&manifest_path).expect("read repository manifest");
    let high_water = fixture
        .root
        .join(".packet28/index/.mapy-v1.generation-high-water.json");
    fs::remove_file(&high_water).expect("simulate a pre-upgrade generation fence");

    let failure =
        clear_index_files_with_generation_fence_hook_for_test(&fixture.root, |temporary| {
            fs::remove_file(temporary).context("unlink owned generation fence temporary file")?;
            fs::write(temporary, br#"{"schema_version":1,"generation":0}"#)
                .context("substitute generation fence temporary file")
        })
        .expect_err("substituted generation fence unexpectedly cleared the repository index");

    assert!(
        failure.to_string().contains("publication binding changed"),
        "clear failed for an unrelated reason: {failure:#}"
    );
    assert_eq!(
        fs::read(&manifest_path).expect("repository manifest survived failed clear"),
        manifest_before,
        "failed fencing deleted or rewrote the live repository publication"
    );
    assert!(
        mapy.exists(),
        "failed fencing deleted the retained repository index"
    );
}

#[cfg(unix)]
#[test]
fn repository_clear_never_lowers_a_concurrently_published_generation_fence() {
    let fixture = IndexFixture::new(&[("src/a.rs", "pub fn alpha() {}\n")]);
    let current = fixture.repo_runtime().manifest.generation;
    let high_water = fixture
        .root
        .join(".packet28/index/.mapy-v1.generation-high-water.json");
    fs::remove_file(&high_water).expect("simulate a pre-upgrade generation fence");
    let concurrent = current.checked_add(1).expect("next concurrent generation");

    clear_index_files_with_generation_fence_hook_for_test(&fixture.root, |temporary| {
        let destination = temporary
            .parent()
            .expect("generation fence temporary has a parent")
            .join(".mapy-v1.generation-high-water.json");
        fs::write(
            destination,
            format!(r#"{{"schema_version":1,"generation":{concurrent}}}"#),
        )
        .context("publish a concurrent higher generation fence")
    })
    .expect("clear around a concurrent higher generation fence");

    let stored: serde_json::Value =
        serde_json::from_slice(&fs::read(&high_water).expect("read concurrent generation fence"))
            .expect("decode concurrent generation fence");
    assert_eq!(stored["generation"].as_u64(), Some(concurrent));
    let rebuilt =
        mapy_core::rebuild_repo_index_runtime(&fixture.root, true).expect("rebuild after clear");
    assert!(
        rebuilt.manifest.generation > concurrent,
        "clear lowered the concurrent generation fence and reused its identity"
    );
}

#[cfg(unix)]
#[test]
fn repository_clear_never_deletes_a_mapy_directory_swapped_after_fencing() {
    let fixture = IndexFixture::new(&[("src/a.rs", "pub fn alpha() {}\n")]);
    let current = fixture.repo_runtime();
    let index = index_dir(&fixture.root);
    let mapy = index.join("mapy-v1");
    let retained = index.join("mapy-v1-retained-during-clear");
    let replacement_generation = current.manifest.generation + 1;
    let replacement_record = format!("generation-{replacement_generation:020}.json");

    let failure = clear_index_files_with_binding_hook_for_test(&fixture.root, || {
        fs::rename(&mapy, &retained).context("retain fenced mapy directory")?;
        fs::create_dir(&mapy).context("create replacement mapy directory")?;
        fs::write(mapy.join(&replacement_record), b"replacement")
            .context("write replacement generation")
    })
    .expect_err("swapped mapy directory unexpectedly cleared successfully");

    assert!(
        failure
            .to_string()
            .contains("failed to remove repository index directory"),
        "clear failed for an unrelated reason: {failure:#}"
    );
    assert_eq!(
        fs::read(mapy.join(&replacement_record)).expect("replacement generation"),
        b"replacement"
    );
    fs::remove_dir_all(&mapy).expect("remove replacement mapy directory");
    fs::rename(&retained, &mapy).expect("restore fenced mapy directory");
}

#[cfg(unix)]
#[test]
fn index_clear_parent_swap_cannot_redirect_the_second_engine_delete() {
    let fixture = IndexFixture::new(&[("src/a.rs", "pub fn alpha() {}\n")]);
    let outside = tempfile::tempdir().expect("create outside directory");
    let outside_regex = outside.path().join("regex-v1");
    fs::create_dir_all(&outside_regex).expect("create outside regex tree");
    let outside_sentinel = outside_regex.join("do-not-delete");
    fs::write(&outside_sentinel, b"outside").expect("write outside sentinel");
    let index = index_dir(&fixture.root);
    let retained = index.with_file_name("index-retained-between-engines");
    let revision =
        persist_index_clear_pending(&fixture.root).expect("persist clear before parent swap");

    let failure = perform_index_clear_with_hook(
        &fixture.state,
        None,
        Some(revision),
        IndexFollowUp::default(),
        || {
            fs::rename(&index, &retained).context("rename retained index directory")?;
            std::os::unix::fs::symlink(outside.path(), &index)
                .context("replace index directory with outside symlink")
        },
    )
    .expect_err("parent swap unexpectedly completed both engine clears");

    assert!(
        outside_sentinel.exists(),
        "regex clear followed the replacement symlink outside Packet28 state"
    );
    fs::remove_file(&index).expect("remove replacement symlink");
    fs::rename(&retained, &index).expect("restore retained index directory");
    assert!(
        index_clear_is_pending(&fixture.root),
        "failed redirected clear acknowledged its durable marker"
    );
    reconcile_failed_index_clear(&fixture.state, &failure).expect("reconcile failed clear");
    perform_index_clear_with_hook(
        &fixture.state,
        None,
        Some(revision),
        IndexFollowUp::default(),
        || Ok(()),
    )
    .expect("retry clear after restoring retained binding");
    assert!(index_clear_is_complete(&fixture.root));
    assert!(outside_sentinel.exists());
}

#[test]
fn failed_clear_retry_is_replaced_by_a_newer_clear_revision() {
    let fixture = IndexFixture::new(&[("src/a.rs", "pub fn alpha() {}\n")]);
    let (ingress, receiver) = IndexIngress::new();
    fixture.state.lock().expect("state").index_tx = ingress;
    enqueue_index_clear(&fixture.state).expect("queue first clear");
    let first = receiver.recv_debounced().expect("receive first clear");
    fail_clear_between_engines(&fixture.state, &first);

    enqueue_index_clear(&fixture.state).expect("queue newer clear");
    let newer = receiver.recv_debounced().expect("receive newer clear");
    let retry = first.merge_newer(newer.clone());
    assert_eq!(retry.clear_epoch, newer.clear_epoch);
    assert!(retry.paths.is_empty());

    assert_eq!(
        process_index_batch_with_recovery(&fixture.state, &retry, None)
            .expect("complete newer clear"),
        IndexBatchStatus::Complete
    );
    assert!(index_clear_is_complete(&fixture.root));
    assert_eq!(
        fixture
            .state
            .lock()
            .expect("state")
            .interactive_index
            .manifest
            .status,
        DaemonIndexState::Missing
    );
}

#[test]
fn failed_clear_preserves_a_newer_path_and_rebuilds_after_clear() {
    let fixture = IndexFixture::new(&[("src/a.rs", "pub fn alpha() {}\n")]);
    let (ingress, receiver) = IndexIngress::new();
    fixture.state.lock().expect("state").index_tx = ingress;
    enqueue_index_clear(&fixture.state).expect("queue clear");
    let clear = receiver.recv_debounced().expect("receive clear");
    fail_clear_between_engines(&fixture.state, &clear);

    fs::write(
        fixture.root.join("src/a.rs"),
        "pub fn replacement_after_clear() {}\n",
    )
    .expect("write newer path");
    enqueue_incremental_index_paths(&fixture.state, &["src/a.rs".to_string()])
        .expect("queue path after failed clear");
    assert!(index_clear_requires_rebuild(&fixture.root));
    let newer = receiver.recv_debounced().expect("receive newer path");
    let retry = clear.merge_newer(newer);
    assert!(retry.clear_epoch.is_some());
    assert_eq!(
        retry.paths.keys().cloned().collect::<Vec<_>>(),
        vec!["src/a.rs"]
    );

    assert_eq!(
        process_index_batch_with_recovery(&fixture.state, &retry, None)
            .expect("clear then rebuild newer path"),
        IndexBatchStatus::Complete
    );
    assert!(!index_clear_is_pending(&fixture.root));
    assert!(!index_clear_is_complete(&fixture.root));
    let guard = fixture.state.lock().expect("state");
    assert_eq!(
        guard.interactive_index.manifest.status,
        DaemonIndexState::Ready
    );
    assert!(guard.interactive_index.repo_is_current());
    assert!(guard.interactive_index.regex_is_current());
    assert!(guard.interactive_index.manifest.dirty_paths.is_empty());
    drop(guard);
    let restarted =
        load_index_runtime_files(&fixture.root, load_index_manifest_file(&fixture.root));
    assert!(!restarted.needs_rebuild());
}

#[test]
fn regex_failure_after_mapy_publication_requeues_full_and_converges() {
    let fixture = IndexFixture::new(&[("src/a.rs", "pub fn alpha() -> usize { 1 }\n")]);
    let original_repo_generation = fixture.repo_runtime().manifest.generation;
    fs::write(
        fixture.root.join("src/a.rs"),
        "pub fn replacement() -> usize { 2 }\n",
    )
    .expect("replace source");
    let (ingress, receiver) = IndexIngress::new();
    fixture.state.lock().expect("state").index_tx = ingress.clone();
    let outcome = enqueue_incremental_index_paths(&fixture.state, &["src/a.rs".to_string()])
        .expect("queue incremental update");
    assert!(!outcome.full);
    let batch = receiver
        .recv_debounced()
        .expect("receive incremental batch");

    let regex_dir = fixture.root.join(".packet28/index/regex-v1");
    fs::remove_dir_all(&regex_dir).expect("remove regex generation");
    fs::write(&regex_dir, b"block regex publication").expect("install regex blocker");

    let status = process_index_batch_with_recovery(&fixture.state, &batch, None)
        .expect("recover partial publication");

    let retry = match status {
        IndexBatchStatus::Retry(retry) => retry,
        IndexBatchStatus::Complete => panic!("partial publication was not retried"),
    };
    assert!(retry.full_rebuild_epoch.is_some());
    assert_eq!(ingress.pending_counts(), (0, false));
    assert!(
        fixture.repo_runtime().manifest.generation > original_repo_generation,
        "published mapy generation was not reloaded into daemon state"
    );
    let restarted =
        load_index_runtime_files(&fixture.root, load_index_manifest_file(&fixture.root));
    assert!(
        restarted.needs_rebuild(),
        "partial publication restart did not retain recovery work"
    );
    fixture.state.lock().expect("state").interactive_index = restarted;

    fs::remove_file(&regex_dir).expect("remove regex blocker");
    assert_eq!(
        process_index_batch_with_recovery(&fixture.state, &retry, None)
            .expect("retry full publication"),
        IndexBatchStatus::Complete
    );

    let guard = fixture.state.lock().expect("state");
    assert_eq!(
        guard.interactive_index.manifest.status,
        DaemonIndexState::Ready
    );
    assert!(guard.interactive_index.manifest.dirty_paths.is_empty());
    assert!(guard.interactive_index.manifest.queued_paths.is_empty());
    assert!(guard.interactive_index.repo_is_current());
    assert!(guard.interactive_index.regex_is_current());
    drop(guard);
    let converged =
        load_index_runtime_files(&fixture.root, load_index_manifest_file(&fixture.root));
    assert!(!converged.needs_rebuild());
}

#[test]
fn daemon_rebuild_and_one_file_update_preserve_parity_without_snapshot_rewrite() {
    let fixture = IndexFixture::new(&[
        ("src/a.rs", "pub fn alpha() -> usize { 1 }\n"),
        ("src/b.rs", "pub fn beta() -> usize { 2 }\n"),
    ]);
    let original_repo = fixture.repo_runtime();
    let original_regex = fixture.regex_runtime();
    let original_generation = fixture
        .state
        .lock()
        .expect("state")
        .interactive_index
        .manifest
        .generation;

    fs::write(
        fixture.root.join("src/a.rs"),
        "pub fn replacement() -> usize { 3 }\n",
    )
    .expect("replace source");
    fixture.update(&["src/a.rs"]);

    let updated_repo = fixture.repo_runtime();
    let updated_regex = fixture.regex_runtime();
    let guard = fixture.state.lock().expect("state");
    assert!(guard.interactive_index.repo_is_current());
    assert!(guard.interactive_index.regex_is_current());
    assert_eq!(
        guard.interactive_index.manifest.generation,
        original_generation + 1
    );
    assert_eq!(guard.interactive_index.manifest.total_files, 2);
    assert_eq!(guard.interactive_index.manifest.indexed_files, 2);
    drop(guard);

    assert!(original_repo.shares_base_with(&updated_repo));
    assert!(original_regex.shares_base_with(&updated_regex));
    assert!(original_repo
        .file("src/a.rs")
        .is_some_and(|entry| entry.symbols.iter().any(|symbol| symbol.name == "alpha")));
    assert!(updated_repo.file("src/a.rs").is_some_and(|entry| entry
        .symbols
        .iter()
        .any(|symbol| symbol.name == "replacement")));
    assert_eq!(
        updated_repo.materialize_snapshot(),
        Some(mapy_core::build_repo_index(&fixture.root, true).expect("parity snapshot"))
    );
    assert_search_parity(&fixture.root, &updated_regex, "replacement");
    assert!(!index_snapshot_path(&fixture.root).exists());
}

#[test]
fn daemon_delete_and_restart_reload_tombstones() {
    let fixture = IndexFixture::new(&[
        ("src/a.rs", "pub fn alpha() -> usize { 1 }\n"),
        ("src/b.rs", "pub fn beta() -> usize { 2 }\n"),
    ]);
    fs::remove_file(fixture.root.join("src/b.rs")).expect("delete source");

    fixture.update(&["src/b.rs"]);

    let repo_runtime = fixture.repo_runtime();
    let regex_runtime = fixture.regex_runtime();
    assert!(repo_runtime.file("src/b.rs").is_none());
    assert_eq!(repo_runtime.manifest.total_files, 1);
    assert_search_parity(&fixture.root, &regex_runtime, "beta");

    let restarted =
        load_index_runtime_files(&fixture.root, load_index_manifest_file(&fixture.root));
    assert!(
        !restarted.needs_rebuild(),
        "restart unexpectedly requested rebuild: daemon={:?}, repo={:?}, regex={:?}",
        restarted.manifest,
        restarted
            .repo_runtime
            .as_ref()
            .map(|runtime| &runtime.manifest),
        restarted
            .regex_runtime
            .as_ref()
            .map(|runtime| &runtime.manifest)
    );
    assert_eq!(
        restarted
            .repo_runtime
            .as_ref()
            .expect("reloaded repo")
            .manifest
            .generation,
        repo_runtime.manifest.generation
    );
    assert!(restarted
        .repo_runtime
        .as_ref()
        .expect("reloaded repo")
        .file("src/b.rs")
        .is_none());
    assert_search_parity(
        &fixture.root,
        restarted.regex_runtime.as_ref().expect("reloaded regex"),
        "beta",
    );
}

#[test]
fn daemon_restart_recovers_corruption_and_forces_a_clean_rebuild() {
    let fixture = IndexFixture::new(&[
        ("src/a.rs", "pub fn alpha() -> usize { 1 }\n"),
        ("src/b.rs", "pub fn beta() -> usize { 2 }\n"),
    ]);
    fs::write(
        fixture.root.join("src/a.rs"),
        "pub fn replacement() -> usize { 3 }\n",
    )
    .expect("replace source");
    fixture.update(&["src/a.rs"]);
    let corrupt_generation = fixture.repo_runtime().manifest.generation;
    let record_path = fixture
        .root
        .join(".packet28/index/mapy-v1")
        .join(format!("generation-{corrupt_generation:020}.json"));
    fs::write(&record_path, b"{").expect("corrupt generation record");

    let restarted =
        load_index_runtime_files(&fixture.root, load_index_manifest_file(&fixture.root));
    assert_ne!(
        restarted.manifest.status,
        DaemonIndexState::Missing,
        "persisted daemon manifest was not reloadable"
    );
    let recovered = restarted.repo_runtime.as_ref().expect("recovered repo");
    assert!(recovered.is_loaded());
    assert_eq!(
        recovered.manifest.recovered_from_generation,
        Some(corrupt_generation)
    );
    assert!(restarted.needs_rebuild());

    {
        let mut guard = fixture.state.lock().expect("state");
        guard.interactive_index = restarted;
    }
    fixture.update(&["src/a.rs"]);

    let repaired = fixture.state.lock().expect("state");
    assert!(repaired.interactive_index.repo_is_current());
    assert!(repaired.interactive_index.regex_is_current());
    assert!(!repaired.interactive_index.needs_rebuild());
    assert!(repaired
        .interactive_index
        .repo_runtime
        .as_ref()
        .expect("repaired repo")
        .file("src/a.rs")
        .is_some_and(|entry| entry
            .symbols
            .iter()
            .any(|symbol| symbol.name == "replacement")));
}

#[test]
fn daemon_generation_swap_keeps_concurrent_old_readers_alive() {
    let fixture = IndexFixture::new(&[
        ("src/a.rs", "pub fn alpha() -> usize { 1 }\n"),
        ("src/b.rs", "pub fn beta() -> usize { 2 }\n"),
    ]);
    let original = fixture.repo_runtime();
    fs::write(
        fixture.root.join("src/a.rs"),
        "pub fn replacement() -> usize { 3 }\n",
    )
    .expect("replace source");
    let barrier = Arc::new(Barrier::new(9));

    std::thread::scope(|scope| {
        let readers = (0..8)
            .map(|_| {
                let runtime = original.clone();
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    barrier.wait();
                    for _ in 0..256 {
                        assert!(runtime.file("src/a.rs").is_some_and(|entry| entry
                            .symbols
                            .iter()
                            .any(|symbol| symbol.name == "alpha")));
                    }
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        fixture.update(&["src/a.rs"]);
        for reader in readers {
            reader.join().expect("reader");
        }
    });

    let updated = fixture.repo_runtime();
    assert!(original.shares_base_with(&updated));
    assert!(updated.file("src/a.rs").is_some_and(|entry| entry
        .symbols
        .iter()
        .any(|symbol| symbol.name == "replacement")));
}

#[test]
fn daemon_clear_removes_generations_and_preserves_owned_readers() {
    let fixture = IndexFixture::new(&[
        ("src/a.rs", "pub fn alpha() -> usize { 1 }\n"),
        ("src/b.rs", "pub fn beta() -> usize { 2 }\n"),
    ]);
    let reader = fixture.repo_runtime();
    fs::write(index_snapshot_path(&fixture.root), b"legacy snapshot").expect("legacy artifact");

    perform_index_clear(&fixture.state).expect("clear");

    let guard = fixture.state.lock().expect("state");
    assert!(guard.interactive_index.repo_runtime.is_none());
    assert!(guard.interactive_index.regex_runtime.is_none());
    drop(guard);
    assert!(!fixture.root.join(".packet28/index/mapy-v1").exists());
    assert!(!fixture.root.join(".packet28/index/regex-v1").exists());
    assert!(!index_snapshot_path(&fixture.root).exists());
    assert!(reader.file("src/a.rs").is_some());
}

#[test]
fn daemon_clear_migrates_legacy_mapy_generation_fencing() {
    let fixture = IndexFixture::new(&[
        ("src/a.rs", "pub fn alpha() -> usize { 1 }\n"),
        ("src/b.rs", "pub fn beta() -> usize { 2 }\n"),
    ]);
    fs::write(
        fixture.root.join("src/c.rs"),
        "pub struct PreUpgradeCurrent;\n",
    )
    .expect("write pre-upgrade current");
    let stale =
        mapy_core::rebuild_repo_index_runtime(&fixture.root, true).expect("pre-upgrade current");
    fs::write(
        fixture.root.join(".packet28/index/mapy-v1/manifest.json"),
        b"{",
    )
    .expect("corrupt pre-upgrade manifest");
    let high_water_path = fixture
        .root
        .join(".packet28/index/.mapy-v1.generation-high-water.json");
    fs::remove_file(&high_water_path).expect("simulate a pre-upgrade mapy index");

    clear_index_files(&fixture.root).expect("clear repository index");

    assert!(!fixture.root.join(".packet28/index/mapy-v1").exists());
    let high_water: serde_json::Value =
        serde_json::from_slice(&fs::read(&high_water_path).expect("migrated high-water"))
            .expect("decode migrated high-water");
    assert_eq!(
        high_water["generation"].as_u64(),
        Some(stale.manifest.generation)
    );

    let rebuilt =
        mapy_core::rebuild_repo_index_runtime(&fixture.root, true).expect("post-clear rebuild");
    assert!(rebuilt.manifest.generation > stale.manifest.generation);
    fs::write(
        fixture.root.join("src/a.rs"),
        "pub fn stale_after_clear() {}\n",
    )
    .expect("change source");
    let error = mapy_core::update_repo_index_runtime(
        &fixture.root,
        &stale,
        &[String::from("src/a.rs")],
        true,
    )
    .expect_err("pre-clear runtime must not publish");
    assert!(error.to_string().contains("generation conflict"));
}

#[test]
fn daemon_incremental_publication_benchmark() {
    const FILE_COUNT: usize = 256;
    const UPDATES: usize = 5;

    let fixture = IndexFixture::generated(FILE_COUNT);
    let original = fixture.repo_runtime();
    let initial_tree = snapshot_tree(&fixture.root.join(".packet28/index"));
    let initial_generation_bytes = tree_bytes(&initial_tree);
    let mut samples = Vec::with_capacity(UPDATES);
    let mut publication_bytes = 0;

    for revision in 1..=UPDATES {
        write_generated_source(&fixture.root, 0, revision);
        let before = snapshot_tree(&fixture.root.join(".packet28/index"));
        let started = Instant::now();
        fixture.update(&["src/file_000.rs"]);
        samples.push(started.elapsed());
        let after = snapshot_tree(&fixture.root.join(".packet28/index"));
        publication_bytes = changed_file_bytes(&before, &after);
    }

    let median = median(samples);
    let updated = fixture.repo_runtime();
    println!(
        "daemon_incremental_median_us={} daemon_incremental_publication_bytes={} daemon_initial_generation_bytes={} legacy_snapshot_written={}",
        median.as_micros(),
        publication_bytes,
        initial_generation_bytes,
        index_snapshot_path(&fixture.root).exists()
    );
    assert!(original.shares_base_with(&updated));
    assert!(
        publication_bytes < initial_generation_bytes,
        "incremental publication {publication_bytes} should be smaller than initial generation {initial_generation_bytes}"
    );
    assert!(!index_snapshot_path(&fixture.root).exists());
}

#[test]
fn broker_map_and_query_consume_incrementally_updated_daemon_runtimes() {
    let fixture = IndexFixture::new(&[("src/a.rs", "pub fn original_symbol() -> usize { 1 }\n")]);
    fs::write(
        fixture.root.join("src/a.rs"),
        "pub fn incremental_symbol() -> usize { 2 }\n",
    )
    .expect("incremental edit");
    fixture.update(&["src/a.rs"]);

    let map = crate::broker::testing::build_repo_map_envelope(
        &fixture.state,
        &fixture.root,
        &[],
        &[String::from("incremental_symbol")],
        8,
        16,
    )
    .expect("broker runtime map");
    let rich_map = mapy_core::expand_repo_map_payload(&map);
    assert!(rich_map
        .symbols_ranked
        .iter()
        .any(|symbol| symbol.name == "incremental_symbol" && symbol.file == "src/a.rs"));

    let snapshot = suite_packet_core::AgentSnapshotPayload::default();
    let request = BrokerGetContextRequest {
        task_id: "task-index-runtime-query".to_string(),
        action: Some(BrokerAction::Inspect),
        query: Some("Where is incremental_symbol defined?".to_string()),
        ..BrokerGetContextRequest::default()
    };
    let query_focus = crate::broker::testing::derive_query_focus(request.query.as_deref());
    let execution = crate::broker::testing::build_reducer_search_execution(
        crate::broker::testing::SearchExecutionArgs {
            state: Some(&fixture.state),
            root: &fixture.root,
            snapshot: &snapshot,
            request: &request,
            query_focus: &query_focus,
            action: BrokerAction::Inspect,
            max_files: 8,
            max_evidence_lines: 8,
        },
    );

    assert!(execution.used_persisted_runtime);
    assert_eq!(
        execution.files.first().map(|file| file.path.as_str()),
        Some("src/a.rs")
    );
    assert!(execution
        .evidence_by_file
        .get("src/a.rs")
        .is_some_and(|evidence| evidence
            .rendered_lines
            .iter()
            .any(|line| line.contains("incremental_symbol"))));
}

#[test]
fn broker_corrupt_runtime_fallback_fails_closed_without_repository_rescan() {
    let fixture = IndexFixture::new(&[("src/a.rs", "pub fn indexed_symbol() -> usize { 1 }\n")]);
    fs::write(
        fixture.root.join(".packet28/index/mapy-v1/manifest.json"),
        b"{",
    )
    .expect("corrupt persisted map manifest");
    fs::write(
        fixture.root.join(".packet28/index/regex-v1/manifest.json"),
        b"{",
    )
    .expect("corrupt persisted regex manifest");
    let repo_runtime =
        mapy_core::load_repo_index_runtime(&fixture.root).expect("reload corrupt map runtime");
    let regex_runtime =
        packet28_search_core::load_runtime(&fixture.root).expect("reload corrupt regex runtime");
    assert!(!repo_runtime.is_loaded());
    assert!(!regex_runtime.is_loaded());
    {
        let mut guard = fixture.state.lock().expect("state");
        guard.interactive_index.repo_runtime = Some(repo_runtime);
        guard.interactive_index.regex_runtime = Some(regex_runtime);
    }
    fs::rename(
        fixture.root.join("src"),
        fixture.root.join("source-unavailable"),
    )
    .expect("make a repository rescan impossible");

    let map_error = crate::broker::testing::build_repo_map_envelope(
        &fixture.state,
        &fixture.root,
        &[],
        &[String::from("indexed_symbol")],
        8,
        16,
    )
    .expect_err("corrupt map runtime must not fall back to a scan");
    assert!(map_error
        .to_string()
        .contains("authenticated daemon repository map runtime is not current"));

    let snapshot = suite_packet_core::AgentSnapshotPayload::default();
    let request = BrokerGetContextRequest {
        task_id: "task-corrupt-index-runtime".to_string(),
        action: Some(BrokerAction::Inspect),
        query: Some("Where is indexed_symbol defined?".to_string()),
        ..BrokerGetContextRequest::default()
    };
    let query_focus = crate::broker::testing::derive_query_focus(request.query.as_deref());
    let execution = crate::broker::testing::build_reducer_search_execution(
        crate::broker::testing::SearchExecutionArgs {
            state: Some(&fixture.state),
            root: &fixture.root,
            snapshot: &snapshot,
            request: &request,
            query_focus: &query_focus,
            action: BrokerAction::Inspect,
            max_files: 8,
            max_evidence_lines: 8,
        },
    );

    assert!(!execution.used_persisted_runtime);
    assert!(execution.files.is_empty());
    assert!(execution.evidence_by_file.is_empty());
}

fn fail_clear_between_engines(state: &Arc<Mutex<DaemonState>>, batch: &IndexWorkBatch) {
    let clear_epoch = batch.clear_epoch.expect("clear epoch");
    let failure = perform_index_clear_with_hook(
        state,
        Some(clear_epoch),
        batch.clear_revision,
        batch.follow_up_after(clear_epoch),
        || anyhow::bail!("injected clear failure"),
    )
    .expect_err("injected clear failure unexpectedly completed");
    reconcile_failed_index_clear(state, &failure).expect("reconcile failed clear");
}

fn assert_search_parity(
    root: &Path,
    runtime: &packet28_search_core::RegexIndexRuntime,
    query: &str,
) {
    let request = packet28_reducer_core::SearchRequest {
        query: query.to_string(),
        fixed_string: true,
        ..packet28_reducer_core::SearchRequest::default()
    };
    let indexed = packet28_search_core::indexed_search(root, runtime, &request).expect("indexed");
    let reducer = packet28_reducer_core::search(root, &request).expect("reducer");
    assert_eq!(indexed.match_count, reducer.match_count);
    assert_eq!(indexed.paths, reducer.paths);
    assert_eq!(indexed.regions, reducer.regions);
}

fn write_generated_source(root: &Path, idx: usize, revision: usize) {
    let mut body = format!(
        "pub fn file_{idx:03}_revision_{revision}(value: usize) -> usize {{\n    value + {revision}\n}}\n"
    );
    for line in 0..32 {
        body.push_str(&format!(
            "pub const FILE_{idx:03}_LINE_{line:02}: &str = \"packet28 daemon incremental benchmark\";\n"
        ));
    }
    let path = root.join(format!("src/file_{idx:03}.rs"));
    fs::create_dir_all(path.parent().expect("source parent")).expect("create source parent");
    fs::write(path, body).expect("write generated source");
}

fn snapshot_tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, path: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(path).expect("read index directory") {
            let entry = entry.expect("index entry");
            let path = entry.path();
            if path.is_dir() {
                visit(root, &path, files);
            } else {
                let relative = path
                    .strip_prefix(root)
                    .expect("relative artifact")
                    .to_path_buf();
                files.insert(relative, fs::read(path).expect("read artifact"));
            }
        }
    }

    let mut files = BTreeMap::new();
    visit(root, root, &mut files);
    files
}

fn changed_file_bytes(
    before: &BTreeMap<PathBuf, Vec<u8>>,
    after: &BTreeMap<PathBuf, Vec<u8>>,
) -> u64 {
    after
        .iter()
        .filter(|(path, bytes)| before.get(*path) != Some(*bytes))
        .map(|(_, bytes)| bytes.len() as u64)
        .sum()
}

fn tree_bytes(tree: &BTreeMap<PathBuf, Vec<u8>>) -> u64 {
    tree.values().map(|bytes| bytes.len() as u64).sum()
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort_unstable();
    samples[samples.len() / 2]
}
