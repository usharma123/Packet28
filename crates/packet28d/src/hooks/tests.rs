use super::*;
use packet28_daemon_protocol::hooks::RelaunchPreference;
use packet28_daemon_protocol::task::TaskRecord;
use std::ops::Deref;

#[test]
fn hook_reducer_cache_is_capped_and_keeps_newest_entries() {
    let mut task = TaskRecord::default();
    // Insert well beyond the cap with strictly increasing timestamps so the
    // newest entries are unambiguous.
    let total = HOOK_REDUCER_CACHE_MAX_ENTRIES + 100;
    for index in 0..total {
        task.hook_reducer_cache.insert(
            format!("fingerprint-{index:05}"),
            HookReducerCacheEntry {
                cache_fingerprint: format!("fingerprint-{index:05}"),
                occurred_at_unix: index as u64,
                ..HookReducerCacheEntry::default()
            },
        );
    }
    prune_hook_reducer_cache(&mut task);

    assert_eq!(
        task.hook_reducer_cache.len(),
        HOOK_REDUCER_CACHE_MAX_ENTRIES
    );
    // The oldest entries were evicted; the newest are retained.
    assert!(task
        .hook_reducer_cache
        .contains_key(&format!("fingerprint-{:05}", total - 1)));
    assert!(!task.hook_reducer_cache.contains_key("fingerprint-00000"));
}

#[test]
fn hook_reducer_cache_prune_is_noop_below_cap() {
    let mut task = TaskRecord::default();
    for index in 0..10u64 {
        task.hook_reducer_cache.insert(
            format!("fp-{index}"),
            HookReducerCacheEntry {
                occurred_at_unix: index,
                ..HookReducerCacheEntry::default()
            },
        );
    }
    prune_hook_reducer_cache(&mut task);
    assert_eq!(task.hook_reducer_cache.len(), 10);
}

struct TestDaemonState {
    state: Arc<Mutex<DaemonState>>,
    _root: tempfile::TempDir,
}

impl Deref for TestDaemonState {
    type Target = Arc<Mutex<DaemonState>>;

    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

fn test_state() -> TestDaemonState {
    let test_root = tempfile::Builder::new()
        .prefix("packet28-hook-test-")
        .tempdir()
        .unwrap();
    let root = test_root.path().to_path_buf();
    ensure_daemon_dir(&root).unwrap();
    let kernel = Arc::new(Kernel::with_v1_reducers_and_persistence(
        PersistConfig::new(root.clone()),
    ));
    let kernel_registry = Arc::new(
        crate::kernel_registry::PersistentKernelRegistry::new(&root, kernel.clone(), 4).unwrap(),
    );
    let (index_tx, index_rx) = IndexIngress::new();
    thread::spawn(move || index_rx.discard_until_shutdown());
    let (background_tx, mut background_rx) = tokio::sync::mpsc::channel(8);
    thread::spawn(move || while background_rx.blocking_recv().is_some() {});
    let (persistence_owner, persistence) = PersistenceOwner::start_for_test(
        root.clone(),
        Duration::from_millis(TASK_PERSISTENCE_DEBOUNCE_MS),
    )
    .unwrap();
    let state = Arc::new(Mutex::new(DaemonState {
        root,
        kernel,
        kernel_registry,
        runtime: DaemonRuntimeInfo::default(),
        tasks: TaskRegistry::default(),
        task_generations: TaskGenerationRegistry::default(),
        agent_snapshots: BTreeMap::new(),
        watches: WatchRegistry::default(),
        registry_instance_id: "test-registry-instance".to_string(),
        registry_page_index: None,
        watcher_handles: HashMap::new(),
        subscribers: HashMap::new(),
        source_file_cache: BTreeMap::new(),
        interactive_index: InteractiveIndexRuntime::default(),
        index_tx,
        background_tx,
        persistence,
        _persistence_owner: Some(persistence_owner),
        shutdown: ShutdownSignal::new(),
        changes: StateChangeSignal::new(),
        shutting_down: false,
    }));
    TestDaemonState {
        state,
        _root: test_root,
    }
}

#[test]
fn test_state_removes_temporary_root_when_dropped() {
    let state = test_state();
    let root = state.lock().unwrap().root.clone();
    let surviving_state_reference = state.clone();

    assert!(root.exists());
    drop(state);
    assert!(!root.exists());
    drop(surviving_state_reference);
}

fn packet(summary: &str) -> packet28_daemon_protocol::hooks::HookReducerPacket {
    packet28_daemon_protocol::hooks::HookReducerPacket {
        packet_type: "packet28.hook.fs.v2".to_string(),
        tool_name: "Bash".to_string(),
        operation_kind: suite_packet_core::ToolOperationKind::Read,
        reducer_family: Some("fs".to_string()),
        canonical_command_kind: Some("fs_cat".to_string()),
        summary: summary.to_string(),
        compact_preview: None,
        command: Some("cat src/lib.rs".to_string()),
        search_query: None,
        compact_path: Some("reducer_rewrite".to_string()),
        passthrough_reason: None,
        raw_est_tokens: Some(10),
        reduced_est_tokens: Some(10),
        paths: vec!["src/lib.rs".to_string()],
        regions: vec!["src/lib.rs:1-3".to_string()],
        symbols: Vec::new(),
        equivalence_key: Some("read:src/lib.rs".to_string()),
        est_tokens: 10,
        est_bytes: 40,
        failed: false,
        error_class: None,
        error_message: None,
        retryable: Some(false),
        duration_ms: Some(12),
        exit_code: Some(0),
        cache_fingerprint: Some("fs:fs_cat:src/lib.rs".to_string()),
        cacheable: Some(true),
        mutation: Some(false),
        raw_artifact_handle: None,
        raw_artifact_available: false,
        artifact: None,
    }
}

fn prepare_handoff_from_hooks(
    state: Arc<Mutex<DaemonState>>,
    task_id: &str,
    boundary_kind: HookBoundaryKind,
    host_budget: Option<u64>,
) -> Result<HookIngestResponse> {
    let root = state.lock().map_err(lock_err)?.root.clone();
    let config = load_hook_runtime_config(&root)?;
    maybe_prepare_handoff_from_hooks(state, task_id, boundary_kind, host_budget, &config)
}

#[test]
fn missing_hook_runtime_config_keeps_ingest_enabled_by_default() {
    let state = test_state();

    let response = hook_ingest(
        state.clone(),
        HookIngestRequest {
            task_id: "task-missing-config".to_string(),
            ..HookIngestRequest::default()
        },
    )
    .unwrap();

    assert!(response.accepted);
}

#[test]
fn malformed_hook_runtime_config_rejects_ingest_without_state_or_byte_mutation() {
    let state = test_state();
    let root = state.lock().unwrap().root.clone();
    let config_path = hook_runtime_config_path(&root);
    let original = b"{\"hooks_enabled\": tru".to_vec();
    fs::write(&config_path, &original).unwrap();
    let before = serde_json::to_value(&state.lock().unwrap().tasks).unwrap();
    let task_id = "task-malformed-config";

    let error = hook_ingest(
        state.clone(),
        HookIngestRequest {
            task_id: task_id.to_string(),
            reducer_packet: Some(packet("must not be ingested")),
            ..HookIngestRequest::default()
        },
    )
    .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("failed to parse hook runtime config"),
        "{error:#}"
    );
    assert_eq!(
        serde_json::to_value(&state.lock().unwrap().tasks).unwrap(),
        before
    );
    assert_eq!(fs::read(config_path).unwrap(), original);
    let storage_id = TaskStorageId::try_from(task_id).unwrap();
    assert!(!task_artifact_dir(&root, &storage_id).exists());
}

#[test]
fn unreadable_hook_runtime_config_rejects_ingest_without_state_mutation() {
    let state = test_state();
    let root = state.lock().unwrap().root.clone();
    let config_path = hook_runtime_config_path(&root);
    fs::create_dir(&config_path).unwrap();
    let marker_path = config_path.join("preserve.bin");
    let original = vec![0x00, 0xff, 0x7f];
    fs::write(&marker_path, &original).unwrap();
    let before = serde_json::to_value(&state.lock().unwrap().tasks).unwrap();

    let error = hook_ingest(
        state.clone(),
        HookIngestRequest {
            task_id: "task-unreadable-config".to_string(),
            reducer_packet: Some(packet("must not be ingested")),
            ..HookIngestRequest::default()
        },
    )
    .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("failed to read hook runtime config"),
        "{error:#}"
    );
    assert_eq!(
        serde_json::to_value(&state.lock().unwrap().tasks).unwrap(),
        before
    );
    assert_eq!(fs::read(marker_path).unwrap(), original);
    assert!(config_path.is_dir());
}

#[test]
fn invalid_utf8_hook_runtime_config_rejects_handoff_without_state_or_byte_mutation() {
    let state = test_state();
    let root = state.lock().unwrap().root.clone();
    let task_id = "task-invalid-utf8-handoff";
    {
        let mut guard = state.lock().unwrap();
        let task = ensure_task_record_mut(&mut guard.tasks, task_id);
        task.hook_window_est_tokens = 10;
        task.hook_window_est_bytes = 40;
    }
    let before = serde_json::to_value(&state.lock().unwrap().tasks).unwrap();
    let config_path = hook_runtime_config_path(&root);
    let original = vec![b'{', b'}', 0xff];
    fs::write(&config_path, &original).unwrap();

    let error = prepare_handoff_from_hooks(state.clone(), task_id, HookBoundaryKind::Stop, None)
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("failed to read hook runtime config"),
        "{error:#}"
    );
    assert_eq!(
        serde_json::to_value(&state.lock().unwrap().tasks).unwrap(),
        before
    );
    assert_eq!(fs::read(config_path).unwrap(), original);
}

#[test]
fn duplicate_cached_packet_does_not_grow_hook_window() {
    let state = test_state();
    let first = hook_ingest(
        state.clone(),
        HookIngestRequest {
            task_id: "task-cache".to_string(),
            reducer_packet: Some(packet("first read")),
            ..HookIngestRequest::default()
        },
    )
    .unwrap();
    assert!(!first.cache_hit);

    let second = hook_ingest(
        state.clone(),
        HookIngestRequest {
            task_id: "task-cache".to_string(),
            reducer_packet: Some(packet("first read")),
            ..HookIngestRequest::default()
        },
    )
    .unwrap();
    assert!(second.cache_hit);

    let task = load_task_record(&state, "task-cache").unwrap();
    assert_eq!(task.hook_window_est_tokens, 10);
    assert_eq!(task.hook_window_est_bytes, 40);
}

#[test]
fn mutation_packets_are_never_cache_hits_or_cache_entries() {
    let state = test_state();
    let mutation = packet28_daemon_protocol::hooks::HookReducerPacket {
        reducer_family: Some("infra".to_string()),
        canonical_command_kind: Some("kubectl_apply".to_string()),
        summary: "deployment.apps/api configured".to_string(),
        command: Some("kubectl apply -f deploy.yaml".to_string()),
        cache_fingerprint: Some("infra:kubectl_apply:deploy".to_string()),
        cacheable: Some(true),
        mutation: Some(true),
        ..packet("kubectl apply")
    };

    let first = hook_ingest(
        state.clone(),
        HookIngestRequest {
            task_id: "task-mutation-cache".to_string(),
            reducer_packet: Some(mutation.clone()),
            ..HookIngestRequest::default()
        },
    )
    .unwrap();
    assert!(!first.cache_hit);

    let second = hook_ingest(
        state.clone(),
        HookIngestRequest {
            task_id: "task-mutation-cache".to_string(),
            reducer_packet: Some(mutation),
            ..HookIngestRequest::default()
        },
    )
    .unwrap();
    assert!(!second.cache_hit);

    let task = load_task_record(&state, "task-mutation-cache").unwrap();
    assert!(task.hook_reducer_cache.is_empty());
}

#[test]
fn infra_mutation_busts_cached_infra_reads() {
    let state = test_state();
    let read = packet28_daemon_protocol::hooks::HookReducerPacket {
        reducer_family: Some("infra".to_string()),
        canonical_command_kind: Some("docker_ps".to_string()),
        summary: "docker ps listed 1 container(s)".to_string(),
        command: Some("docker ps".to_string()),
        cache_fingerprint: Some("infra:docker_ps".to_string()),
        cacheable: Some(true),
        mutation: Some(false),
        ..packet("docker ps")
    };
    let first = hook_ingest(
        state.clone(),
        HookIngestRequest {
            task_id: "task-infra-epoch".to_string(),
            reducer_packet: Some(read.clone()),
            ..HookIngestRequest::default()
        },
    )
    .unwrap();
    assert!(!first.cache_hit);
    let cached = hook_ingest(
        state.clone(),
        HookIngestRequest {
            task_id: "task-infra-epoch".to_string(),
            reducer_packet: Some(read.clone()),
            ..HookIngestRequest::default()
        },
    )
    .unwrap();
    assert!(cached.cache_hit);

    let mutation = packet28_daemon_protocol::hooks::HookReducerPacket {
        reducer_family: Some("infra".to_string()),
        canonical_command_kind: Some("docker_run".to_string()),
        summary: "docker run completed".to_string(),
        command: Some("docker run alpine echo hi".to_string()),
        cache_fingerprint: Some("infra:docker_run".to_string()),
        cacheable: Some(false),
        mutation: Some(true),
        ..packet("docker run")
    };
    let _ = hook_ingest(
        state.clone(),
        HookIngestRequest {
            task_id: "task-infra-epoch".to_string(),
            reducer_packet: Some(mutation),
            ..HookIngestRequest::default()
        },
    )
    .unwrap();

    let after_mutation = hook_ingest(
        state.clone(),
        HookIngestRequest {
            task_id: "task-infra-epoch".to_string(),
            reducer_packet: Some(read),
            ..HookIngestRequest::default()
        },
    )
    .unwrap();
    assert!(!after_mutation.cache_hit);
}

#[test]
fn remote_state_cache_entries_expire() {
    let state = test_state();
    let read = packet28_daemon_protocol::hooks::HookReducerPacket {
        reducer_family: Some("infra".to_string()),
        canonical_command_kind: Some("aws_sts_get_caller_identity".to_string()),
        summary: "aws caller arn:aws:iam::123:user/demo".to_string(),
        command: Some("aws sts get-caller-identity".to_string()),
        cache_fingerprint: Some("infra:aws_sts".to_string()),
        cacheable: Some(true),
        mutation: Some(false),
        ..packet("aws sts")
    };
    let first = hook_ingest(
        state.clone(),
        HookIngestRequest {
            task_id: "task-remote-ttl".to_string(),
            reducer_packet: Some(read.clone()),
            ..HookIngestRequest::default()
        },
    )
    .unwrap();
    assert!(!first.cache_hit);

    {
        let mut guard = state.lock().unwrap();
        let task = guard.tasks.tasks.get_mut("task-remote-ttl").unwrap();
        let entry = task.hook_reducer_cache.get_mut("infra:aws_sts").unwrap();
        entry.occurred_at_unix = now_unix().saturating_sub(github_cache_ttl_secs() + 1);
    }

    let after_ttl = hook_ingest(
        state.clone(),
        HookIngestRequest {
            task_id: "task-remote-ttl".to_string(),
            reducer_packet: Some(read),
            ..HookIngestRequest::default()
        },
    )
    .unwrap();
    assert!(!after_ttl.cache_hit);
}

#[test]
fn edit_invalidation_busts_fs_cache() {
    let state = test_state();
    let _ = hook_ingest(
        state.clone(),
        HookIngestRequest {
            task_id: "task-edit".to_string(),
            reducer_packet: Some(packet("first read")),
            ..HookIngestRequest::default()
        },
    )
    .unwrap();
    let cached = hook_ingest(
        state.clone(),
        HookIngestRequest {
            task_id: "task-edit".to_string(),
            reducer_packet: Some(packet("first read")),
            ..HookIngestRequest::default()
        },
    )
    .unwrap();
    assert!(cached.cache_hit);

    let _ = hook_ingest(
        state.clone(),
        HookIngestRequest {
            task_id: "task-edit".to_string(),
            reducer_packet: Some(packet28_daemon_protocol::hooks::HookReducerPacket {
                packet_type: "packet28.hook.edit.v1".to_string(),
                tool_name: "Edit".to_string(),
                operation_kind: suite_packet_core::ToolOperationKind::Edit,
                reducer_family: Some("claude_native".to_string()),
                canonical_command_kind: Some("edit".to_string()),
                summary: "edited src/lib.rs".to_string(),
                compact_preview: None,
                command: None,
                search_query: None,
                compact_path: Some("native_tool".to_string()),
                passthrough_reason: None,
                raw_est_tokens: Some(5),
                reduced_est_tokens: Some(5),
                paths: vec!["src/lib.rs".to_string()],
                regions: vec!["src/lib.rs:1-1".to_string()],
                symbols: Vec::new(),
                equivalence_key: None,
                est_tokens: 5,
                est_bytes: 20,
                failed: false,
                error_class: None,
                error_message: None,
                retryable: Some(false),
                duration_ms: Some(5),
                exit_code: Some(0),
                cache_fingerprint: None,
                cacheable: Some(false),
                mutation: Some(true),
                raw_artifact_handle: None,
                raw_artifact_available: false,
                artifact: None,
            }),
            ..HookIngestRequest::default()
        },
    )
    .unwrap();

    let after_edit = hook_ingest(
        state.clone(),
        HookIngestRequest {
            task_id: "task-edit".to_string(),
            reducer_packet: Some(packet("first read")),
            ..HookIngestRequest::default()
        },
    )
    .unwrap();
    assert!(!after_edit.cache_hit);
}

#[test]
fn failed_edit_does_not_bust_fs_cache() {
    let state = test_state();
    let _ = hook_ingest(
        state.clone(),
        HookIngestRequest {
            task_id: "task-failed-edit".to_string(),
            reducer_packet: Some(packet("first read")),
            ..HookIngestRequest::default()
        },
    )
    .unwrap();
    let cached = hook_ingest(
        state.clone(),
        HookIngestRequest {
            task_id: "task-failed-edit".to_string(),
            reducer_packet: Some(packet("first read")),
            ..HookIngestRequest::default()
        },
    )
    .unwrap();
    assert!(cached.cache_hit);

    let _ = hook_ingest(
        state.clone(),
        HookIngestRequest {
            task_id: "task-failed-edit".to_string(),
            reducer_packet: Some(packet28_daemon_protocol::hooks::HookReducerPacket {
                packet_type: "packet28.hook.edit.failure.v1".to_string(),
                tool_name: "Edit".to_string(),
                operation_kind: suite_packet_core::ToolOperationKind::Edit,
                reducer_family: Some("claude_native".to_string()),
                canonical_command_kind: Some("edit".to_string()),
                summary: "edit failed for src/lib.rs: permission denied".to_string(),
                compact_preview: None,
                command: None,
                search_query: None,
                compact_path: Some("native_tool".to_string()),
                passthrough_reason: None,
                raw_est_tokens: Some(5),
                reduced_est_tokens: Some(5),
                paths: vec!["src/lib.rs".to_string()],
                regions: Vec::new(),
                symbols: Vec::new(),
                equivalence_key: None,
                est_tokens: 5,
                est_bytes: 20,
                failed: true,
                error_class: Some("tool_error".to_string()),
                error_message: Some("permission denied".to_string()),
                retryable: Some(false),
                duration_ms: Some(5),
                exit_code: Some(1),
                cache_fingerprint: None,
                cacheable: Some(false),
                mutation: Some(false),
                raw_artifact_handle: None,
                raw_artifact_available: false,
                artifact: None,
            }),
            ..HookIngestRequest::default()
        },
    )
    .unwrap();

    let after_failed_edit = hook_ingest(
        state.clone(),
        HookIngestRequest {
            task_id: "task-failed-edit".to_string(),
            reducer_packet: Some(packet("first read")),
            ..HookIngestRequest::default()
        },
    )
    .unwrap();
    assert!(after_failed_edit.cache_hit);
}

#[test]
fn edit_invalidation_busts_git_cache() {
    let state = test_state();
    let git_packet = packet28_daemon_protocol::hooks::HookReducerPacket {
        packet_type: "packet28.hook.git.v2".to_string(),
        tool_name: "Bash".to_string(),
        operation_kind: suite_packet_core::ToolOperationKind::Git,
        reducer_family: Some("git".to_string()),
        canonical_command_kind: Some("git_status".to_string()),
        summary: "git status reported 1 changed entry".to_string(),
        compact_preview: None,
        command: Some("git status --short src/lib.rs".to_string()),
        search_query: None,
        compact_path: Some("reducer_rewrite".to_string()),
        passthrough_reason: None,
        raw_est_tokens: Some(8),
        reduced_est_tokens: Some(8),
        paths: vec!["src/lib.rs".to_string()],
        regions: Vec::new(),
        symbols: Vec::new(),
        equivalence_key: None,
        est_tokens: 8,
        est_bytes: 32,
        failed: false,
        error_class: None,
        error_message: None,
        retryable: Some(false),
        duration_ms: Some(5),
        exit_code: Some(0),
        cache_fingerprint: Some(
            "git:git_status:git\u{1f}status\u{1f}--short\u{1f}src/lib.rs".to_string(),
        ),
        cacheable: Some(true),
        mutation: Some(false),
        raw_artifact_handle: None,
        raw_artifact_available: false,
        artifact: None,
    };
    let first = hook_ingest(
        state.clone(),
        HookIngestRequest {
            task_id: "task-git-edit".to_string(),
            reducer_packet: Some(git_packet.clone()),
            ..HookIngestRequest::default()
        },
    )
    .unwrap();
    assert!(!first.cache_hit);

    let second = hook_ingest(
        state.clone(),
        HookIngestRequest {
            task_id: "task-git-edit".to_string(),
            reducer_packet: Some(git_packet.clone()),
            ..HookIngestRequest::default()
        },
    )
    .unwrap();
    assert!(second.cache_hit);

    let _ = hook_ingest(
        state.clone(),
        HookIngestRequest {
            task_id: "task-git-edit".to_string(),
            reducer_packet: Some(packet28_daemon_protocol::hooks::HookReducerPacket {
                packet_type: "packet28.hook.edit.v1".to_string(),
                tool_name: "Edit".to_string(),
                operation_kind: suite_packet_core::ToolOperationKind::Edit,
                reducer_family: Some("claude_native".to_string()),
                canonical_command_kind: Some("edit".to_string()),
                summary: "edited src/lib.rs".to_string(),
                compact_preview: None,
                command: None,
                search_query: None,
                compact_path: Some("native_tool".to_string()),
                passthrough_reason: None,
                raw_est_tokens: Some(5),
                reduced_est_tokens: Some(5),
                paths: vec!["src/lib.rs".to_string()],
                regions: vec!["src/lib.rs:1-1".to_string()],
                symbols: Vec::new(),
                equivalence_key: None,
                est_tokens: 5,
                est_bytes: 20,
                failed: false,
                error_class: None,
                error_message: None,
                retryable: Some(false),
                duration_ms: Some(5),
                exit_code: Some(0),
                cache_fingerprint: None,
                cacheable: Some(false),
                mutation: Some(true),
                raw_artifact_handle: None,
                raw_artifact_available: false,
                artifact: None,
            }),
            ..HookIngestRequest::default()
        },
    )
    .unwrap();

    let after_edit = hook_ingest(
        state.clone(),
        HookIngestRequest {
            task_id: "task-git-edit".to_string(),
            reducer_packet: Some(git_packet),
            ..HookIngestRequest::default()
        },
    )
    .unwrap();
    assert!(!after_edit.cache_hit);
}

#[test]
fn successful_handoff_preparation_clears_hook_window() {
    let state = test_state();
    let _ = hook_ingest(
        state.clone(),
        HookIngestRequest {
            task_id: "task-handoff-reset".to_string(),
            reducer_packet: Some(packet("first read")),
            ..HookIngestRequest::default()
        },
    )
    .unwrap();
    let _ = broker_write_state(
        state.clone(),
        BrokerWriteStateRequest {
            task_id: "task-handoff-reset".to_string(),
            op: Some(BrokerWriteOp::Intention),
            text: Some("Investigate hook handoff reset".to_string()),
            refresh_context: Some(false),
            ..BrokerWriteStateRequest::default()
        },
    )
    .unwrap();

    let response = prepare_handoff_from_hooks(
        state.clone(),
        "task-handoff-reset",
        HookBoundaryKind::Stop,
        None,
    )
    .unwrap();

    assert!(response.handoff_ready);
    let task = load_task_record(&state, "task-handoff-reset").unwrap();
    assert_eq!(task.hook_window_est_tokens, 0);
    assert_eq!(task.hook_window_est_bytes, 0);
    assert!(!task.hook_threshold_exceeded);
}

#[test]
fn graduated_threshold_levels_are_computed_correctly() {
    let config = HookRuntimeConfig {
        context_budget_tokens: 1000,
        warn_threshold_fraction: 0.6,
        prepare_threshold_fraction: 0.75,
        force_threshold_fraction: 0.9,
        ..HookRuntimeConfig::default()
    };
    assert_eq!(
        config.compute_threshold_level(0, 1000),
        ThresholdLevel::Normal
    );
    assert_eq!(
        config.compute_threshold_level(599, 1000),
        ThresholdLevel::Normal
    );
    assert_eq!(
        config.compute_threshold_level(600, 1000),
        ThresholdLevel::Warn
    );
    assert_eq!(
        config.compute_threshold_level(749, 1000),
        ThresholdLevel::Warn
    );
    assert_eq!(
        config.compute_threshold_level(750, 1000),
        ThresholdLevel::Prepare
    );
    assert_eq!(
        config.compute_threshold_level(899, 1000),
        ThresholdLevel::Prepare
    );
    assert_eq!(
        config.compute_threshold_level(900, 1000),
        ThresholdLevel::Force
    );
    assert_eq!(
        config.compute_threshold_level(1500, 1000),
        ThresholdLevel::Force
    );
}

#[test]
fn host_budget_override_changes_effective_budget() {
    let config = HookRuntimeConfig {
        context_budget_tokens: 1000,
        ..HookRuntimeConfig::default()
    };
    assert_eq!(config.effective_budget(None), 1000);
    assert_eq!(config.effective_budget(Some(5000)), 5000);
    // Zero is ignored (falls back to config).
    assert_eq!(config.effective_budget(Some(0)), 1000);
}

#[test]
fn threshold_accumulation_triggers_exceeded_without_stop_boundary() {
    let state = test_state();
    // Write a hook runtime config with small budget so threshold fires.
    let root = state.lock().unwrap().root.clone();
    let config = HookRuntimeConfig {
        context_budget_tokens: 100,
        warn_threshold_fraction: 0.6,
        prepare_threshold_fraction: 0.75,
        force_threshold_fraction: 0.9,
        ..HookRuntimeConfig::default()
    };
    let config_path = packet28_daemon_protocol::paths::hook_runtime_config_path(&root);
    std::fs::write(&config_path, serde_json::to_string_pretty(&config).unwrap()).unwrap();

    // Ingest packets totaling 80 tokens (above prepare=75 threshold).
    for i in 0..8 {
        let mut pkt = packet(&format!("read {i}"));
        pkt.est_tokens = 10;
        pkt.cache_fingerprint = Some(format!("unique-fp-{i}"));
        let _ = hook_ingest(
            state.clone(),
            HookIngestRequest {
                task_id: "task-threshold".to_string(),
                reducer_packet: Some(pkt),
                ..HookIngestRequest::default()
            },
        )
        .unwrap();
    }

    let task = load_task_record(&state, "task-threshold").unwrap();
    assert_eq!(task.hook_window_est_tokens, 80);
    assert!(task.hook_threshold_exceeded);

    // Without intention, stop should be blocked.
    let response = prepare_handoff_from_hooks(
        state.clone(),
        "task-threshold",
        HookBoundaryKind::Stop,
        None,
    )
    .unwrap();
    assert!(response.block_stop);
    assert!(!response.handoff_ready);

    // Write intention, then handoff should fire without a boundary.
    let _ = broker_write_state(
        state.clone(),
        BrokerWriteStateRequest {
            task_id: "task-threshold".to_string(),
            op: Some(BrokerWriteOp::Intention),
            text: Some("Continue investigating".to_string()),
            refresh_context: Some(false),
            ..BrokerWriteStateRequest::default()
        },
    )
    .unwrap();

    // Now at Stop boundary with intention → handoff should be ready.
    let response = prepare_handoff_from_hooks(
        state.clone(),
        "task-threshold",
        HookBoundaryKind::Stop,
        None,
    )
    .unwrap();
    assert!(response.handoff_ready);
    assert!(matches!(
        response.threshold_level,
        ThresholdLevel::Prepare | ThresholdLevel::Force
    ));

    // Window should be cleared after successful handoff.
    let task = load_task_record(&state, "task-threshold").unwrap();
    assert_eq!(task.hook_window_est_tokens, 0);
    assert!(!task.hook_threshold_exceeded);
}

#[test]
fn threshold_level_returned_in_response() {
    let state = test_state();
    let root = state.lock().unwrap().root.clone();
    let config = HookRuntimeConfig {
        context_budget_tokens: 100,
        warn_threshold_fraction: 0.6,
        prepare_threshold_fraction: 0.75,
        force_threshold_fraction: 0.9,
        ..HookRuntimeConfig::default()
    };
    let config_path = packet28_daemon_protocol::paths::hook_runtime_config_path(&root);
    std::fs::write(&config_path, serde_json::to_string_pretty(&config).unwrap()).unwrap();

    // Under warn threshold.
    let mut pkt = packet("small read");
    pkt.est_tokens = 50;
    pkt.cache_fingerprint = Some("unique-level-1".to_string());
    let response = hook_ingest(
        state.clone(),
        HookIngestRequest {
            task_id: "task-level".to_string(),
            reducer_packet: Some(pkt),
            ..HookIngestRequest::default()
        },
    )
    .unwrap();
    assert_eq!(response.threshold_level, ThresholdLevel::Normal);

    // Push past warn (60 = 0.6 * 100) → total 65 tokens.
    let mut pkt2 = packet("more read");
    pkt2.est_tokens = 15;
    pkt2.cache_fingerprint = Some("unique-level-2".to_string());
    let response = hook_ingest(
        state.clone(),
        HookIngestRequest {
            task_id: "task-level".to_string(),
            reducer_packet: Some(pkt2),
            ..HookIngestRequest::default()
        },
    )
    .unwrap();
    assert_eq!(response.threshold_level, ThresholdLevel::Warn);

    // Push past force (90 = 0.9 * 100) → total 95 tokens.
    let mut pkt3 = packet("big read");
    pkt3.est_tokens = 30;
    pkt3.cache_fingerprint = Some("unique-level-3".to_string());
    let response = hook_ingest(
        state.clone(),
        HookIngestRequest {
            task_id: "task-level".to_string(),
            reducer_packet: Some(pkt3),
            ..HookIngestRequest::default()
        },
    )
    .unwrap();
    assert_eq!(response.threshold_level, ThresholdLevel::Force);
}

#[test]
fn host_budget_override_affects_threshold_calculation() {
    let state = test_state();
    // Default config has budget=200_000 so threshold is very high.
    // But host override sets budget=100 → threshold fires at 75 tokens.
    let mut pkt = packet("big read");
    pkt.est_tokens = 80;
    pkt.cache_fingerprint = Some("unique-host-1".to_string());
    let response = hook_ingest(
        state.clone(),
        HookIngestRequest {
            task_id: "task-host-budget".to_string(),
            reducer_packet: Some(pkt),
            host_context_budget_tokens: Some(100),
            ..HookIngestRequest::default()
        },
    )
    .unwrap();
    // 80 >= 75 (0.75 * 100), so threshold is exceeded at Prepare level.
    assert!(matches!(
        response.threshold_level,
        ThresholdLevel::Prepare | ThresholdLevel::Force
    ));

    let task = load_task_record(&state, "task-host-budget").unwrap();
    assert!(task.hook_threshold_exceeded);
}

#[test]
fn relaunch_preference_host_managed_is_default() {
    let config = HookRuntimeConfig::default();
    assert_eq!(config.relaunch_preference, RelaunchPreference::HostManaged);
    assert!(!config.daemon_relaunch_enabled());
}

#[test]
fn relaunch_requested_when_daemon_managed_with_command() {
    let state = test_state();
    let root = state.lock().unwrap().root.clone();
    let config = HookRuntimeConfig {
        context_budget_tokens: 100,
        relaunch_preference: RelaunchPreference::DaemonManaged,
        // Use a harmless command that will fail quickly (fine for test).
        relaunch_command: vec!["true".to_string()],
        ..HookRuntimeConfig::default()
    };
    let config_path = packet28_daemon_protocol::paths::hook_runtime_config_path(&root);
    std::fs::write(&config_path, serde_json::to_string_pretty(&config).unwrap()).unwrap();

    // Ingest enough to exceed threshold.
    let mut pkt = packet("big read");
    pkt.est_tokens = 80;
    pkt.cache_fingerprint = Some("unique-relaunch-1".to_string());
    let _ = hook_ingest(
        state.clone(),
        HookIngestRequest {
            task_id: "task-relaunch".to_string(),
            reducer_packet: Some(pkt),
            ..HookIngestRequest::default()
        },
    )
    .unwrap();

    // Write intention so handoff can proceed.
    let _ = broker_write_state(
        state.clone(),
        BrokerWriteStateRequest {
            task_id: "task-relaunch".to_string(),
            op: Some(BrokerWriteOp::Intention),
            text: Some("Continue work".to_string()),
            refresh_context: Some(false),
            ..BrokerWriteStateRequest::default()
        },
    )
    .unwrap();

    // Stop boundary should trigger handoff + relaunch.
    let response =
        prepare_handoff_from_hooks(state.clone(), "task-relaunch", HookBoundaryKind::Stop, None)
            .unwrap();
    assert!(response.handoff_ready);
    assert!(response.relaunch_requested);
    assert_eq!(
        response.relaunch_preference,
        RelaunchPreference::DaemonManaged
    );
}

/// End-to-end test: hook ingest → graduated thresholds → intention write
/// → stop boundary handoff → window reset → brief artifact persisted.
#[test]
fn e2e_hook_threshold_handoff_cycle() {
    let state = test_state();
    let root = state.lock().unwrap().root.clone();
    let config = HookRuntimeConfig {
        context_budget_tokens: 100,
        warn_threshold_fraction: 0.6,
        prepare_threshold_fraction: 0.75,
        force_threshold_fraction: 0.9,
        relaunch_preference: RelaunchPreference::DaemonManaged,
        relaunch_command: vec!["true".to_string()],
        ..HookRuntimeConfig::default()
    };
    let config_path = packet28_daemon_protocol::paths::hook_runtime_config_path(&root);
    std::fs::write(&config_path, serde_json::to_string_pretty(&config).unwrap()).unwrap();

    let task_id = "task-e2e-cycle";

    // Phase 1: Ingest hooks, observe graduated threshold levels.
    // 30 tokens → Normal.
    let mut pkt1 = packet("read file A");
    pkt1.est_tokens = 30;
    pkt1.cache_fingerprint = Some("e2e-fp-1".to_string());
    let r1 = hook_ingest(
        state.clone(),
        HookIngestRequest {
            task_id: task_id.to_string(),
            reducer_packet: Some(pkt1),
            ..HookIngestRequest::default()
        },
    )
    .unwrap();
    assert_eq!(r1.threshold_level, ThresholdLevel::Normal);
    assert!(!r1.handoff_ready);

    // 65 tokens total → Warn.
    let mut pkt2 = packet("read file B");
    pkt2.est_tokens = 35;
    pkt2.cache_fingerprint = Some("e2e-fp-2".to_string());
    let r2 = hook_ingest(
        state.clone(),
        HookIngestRequest {
            task_id: task_id.to_string(),
            reducer_packet: Some(pkt2),
            ..HookIngestRequest::default()
        },
    )
    .unwrap();
    assert_eq!(r2.threshold_level, ThresholdLevel::Warn);

    // 95 tokens total → Force.
    let mut pkt3 = packet("read file C");
    pkt3.est_tokens = 30;
    pkt3.cache_fingerprint = Some("e2e-fp-3".to_string());
    let r3 = hook_ingest(
        state.clone(),
        HookIngestRequest {
            task_id: task_id.to_string(),
            reducer_packet: Some(pkt3),
            ..HookIngestRequest::default()
        },
    )
    .unwrap();
    assert_eq!(r3.threshold_level, ThresholdLevel::Force);

    // Phase 2: Stop without intention → blocked.
    let blocked =
        prepare_handoff_from_hooks(state.clone(), task_id, HookBoundaryKind::Stop, None).unwrap();
    assert!(blocked.block_stop);
    assert!(!blocked.handoff_ready);

    // Phase 3: Write intention.
    let _ = broker_write_state(
        state.clone(),
        BrokerWriteStateRequest {
            task_id: task_id.to_string(),
            op: Some(BrokerWriteOp::Intention),
            text: Some("Refactor auth middleware for compliance".to_string()),
            refresh_context: Some(false),
            ..BrokerWriteStateRequest::default()
        },
    )
    .unwrap();

    // Phase 4: Stop with intention → handoff fires, relaunch queued.
    let handoff =
        prepare_handoff_from_hooks(state.clone(), task_id, HookBoundaryKind::Stop, None).unwrap();
    assert!(handoff.handoff_ready);
    assert!(handoff.relaunch_requested);
    assert_eq!(
        handoff.relaunch_preference,
        RelaunchPreference::DaemonManaged
    );
    assert!(matches!(handoff.threshold_level, ThresholdLevel::Force));

    // Phase 5: Verify window reset.
    let task = load_task_record(&state, task_id).unwrap();
    assert_eq!(task.hook_window_est_tokens, 0);
    assert_eq!(task.hook_window_est_bytes, 0);
    assert!(!task.hook_threshold_exceeded);

    // Phase 6: Verify brief artifact was persisted.
    let storage_id = crate::TaskStorageId::try_from(task_id).unwrap();
    let brief_path = crate::task_brief_markdown_path(&root, &storage_id);
    assert!(
        brief_path.exists(),
        "brief.md should be written after handoff"
    );
    let brief_content = std::fs::read_to_string(&brief_path).unwrap();
    assert!(!brief_content.is_empty(), "brief should not be empty");
}

#[test]
fn relaunch_not_requested_when_host_managed() {
    let state = test_state();
    let root = state.lock().unwrap().root.clone();
    let config = HookRuntimeConfig {
        context_budget_tokens: 100,
        relaunch_preference: RelaunchPreference::HostManaged,
        relaunch_command: vec!["true".to_string()],
        ..HookRuntimeConfig::default()
    };
    let config_path = packet28_daemon_protocol::paths::hook_runtime_config_path(&root);
    std::fs::write(&config_path, serde_json::to_string_pretty(&config).unwrap()).unwrap();

    let mut pkt = packet("big read");
    pkt.est_tokens = 80;
    pkt.cache_fingerprint = Some("unique-host-managed-1".to_string());
    let _ = hook_ingest(
        state.clone(),
        HookIngestRequest {
            task_id: "task-host-managed".to_string(),
            reducer_packet: Some(pkt),
            ..HookIngestRequest::default()
        },
    )
    .unwrap();

    let _ = broker_write_state(
        state.clone(),
        BrokerWriteStateRequest {
            task_id: "task-host-managed".to_string(),
            op: Some(BrokerWriteOp::Intention),
            text: Some("Continue work".to_string()),
            refresh_context: Some(false),
            ..BrokerWriteStateRequest::default()
        },
    )
    .unwrap();

    let response = prepare_handoff_from_hooks(
        state.clone(),
        "task-host-managed",
        HookBoundaryKind::Stop,
        None,
    )
    .unwrap();
    assert!(response.handoff_ready);
    assert!(!response.relaunch_requested);
    assert_eq!(
        response.relaunch_preference,
        RelaunchPreference::HostManaged
    );
}

fn seed_relaunch_ready_task(
    state: &Arc<Mutex<DaemonState>>,
    task_id: &str,
    config: &HookRuntimeConfig,
) {
    let root = state.lock().unwrap().root.clone();
    let config_path = packet28_daemon_protocol::paths::hook_runtime_config_path(&root);
    std::fs::write(&config_path, serde_json::to_string_pretty(config).unwrap()).unwrap();

    let mut pkt = packet("big read");
    pkt.est_tokens = 80;
    pkt.cache_fingerprint = Some(format!("unique-{task_id}"));
    let _ = hook_ingest(
        state.clone(),
        HookIngestRequest {
            task_id: task_id.to_string(),
            reducer_packet: Some(pkt),
            ..HookIngestRequest::default()
        },
    )
    .unwrap();
    let _ = broker_write_state(
        state.clone(),
        BrokerWriteStateRequest {
            task_id: task_id.to_string(),
            op: Some(BrokerWriteOp::Intention),
            text: Some("Continue work".to_string()),
            refresh_context: Some(false),
            ..BrokerWriteStateRequest::default()
        },
    )
    .unwrap();
}

/// Regression: a subagent finishing must never relaunch a fresh worker. The
/// parent session is still alive, so spawning `claude --continue` here hijacks
/// the live conversation and multiplies processes on multi-agent runs.
#[test]
fn relaunch_not_requested_on_subagent_stop() {
    let state = test_state();
    let config = HookRuntimeConfig {
        context_budget_tokens: 100,
        relaunch_preference: RelaunchPreference::DaemonManaged,
        relaunch_command: vec!["true".to_string()],
        ..HookRuntimeConfig::default()
    };
    seed_relaunch_ready_task(&state, "task-subagent-stop", &config);

    let response = prepare_handoff_from_hooks(
        state.clone(),
        "task-subagent-stop",
        HookBoundaryKind::SubagentStop,
        None,
    )
    .unwrap();
    assert!(response.handoff_ready);
    assert!(
        !response.relaunch_requested,
        "SubagentStop must not queue a daemon relaunch"
    );
}

/// Regression: the setup-generated `claude --continue` command (and the older
/// `packet28-agent` wrapper) are not valid headless relaunch targets. Stale
/// configs written by earlier releases must not trigger a relaunch even when
/// the preference still says daemon-managed.
#[test]
fn relaunch_not_requested_for_legacy_generated_command() {
    let state = test_state();
    let config = HookRuntimeConfig {
        context_budget_tokens: 100,
        relaunch_preference: RelaunchPreference::DaemonManaged,
        relaunch_command: vec!["claude".to_string(), "--continue".to_string()],
        ..HookRuntimeConfig::default()
    };
    seed_relaunch_ready_task(&state, "task-legacy-relaunch", &config);

    let response = prepare_handoff_from_hooks(
        state.clone(),
        "task-legacy-relaunch",
        HookBoundaryKind::Stop,
        None,
    )
    .unwrap();
    assert!(response.handoff_ready);
    assert!(
        !response.relaunch_requested,
        "legacy generated `claude --continue` must not be relaunched by the daemon"
    );
}
