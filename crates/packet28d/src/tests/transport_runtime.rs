use super::support::{daemon_test_state, insert_admitted_task_record, shutdown_test_persistence};
use super::*;
use fs2::FileExt;
use packet28_daemon_core::storage::load_task_registry;
use packet28_daemon_protocol::message::{DaemonTransportAuth, DAEMON_TRANSPORT_SECRET_BYTES};
use packet28_daemon_protocol::paths::workspace_socket_path;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

static TCP_TRANSPORT_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn shutdown_waiter(signal: ShutdownSignal) -> tokio::task::JoinHandle<Result<()>> {
    tokio::spawn(async move {
        let mut receiver = signal.subscribe();
        while !*receiver.borrow() {
            receiver
                .changed()
                .await
                .map_err(|_| anyhow!("test shutdown signal closed"))?;
        }
        Ok(())
    })
}

async fn exchange<S>(stream: &mut S, request: &DaemonRequest) -> DaemonResponse
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    exchange_value(stream, request).await
}

async fn exchange_value<S, T>(stream: &mut S, value: &T) -> DaemonResponse
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: serde::Serialize,
{
    let mut encoded = Vec::new();
    write_frame(&mut encoded, value).unwrap();
    stream.write_all(&encoded).await.unwrap();

    let mut header = [0_u8; 8];
    stream.read_exact(&mut header).await.unwrap();
    let len = usize::try_from(u64::from_be_bytes(header)).unwrap();
    let mut body = vec![0_u8; len];
    stream.read_exact(&mut body).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[test]
fn bound_unix_socket_is_owner_only_and_owned_by_effective_user() {
    let directory = tempfile::TempDir::new().unwrap();
    let socket = directory.path().join("packet28d.sock");

    let listener = bind_unix_listener(&socket).unwrap();

    let metadata = std::fs::symlink_metadata(&socket).unwrap();
    assert!(metadata.file_type().is_socket());
    assert_eq!(
        (metadata.uid(), metadata.permissions().mode() & 0o777),
        (effective_uid(), 0o600)
    );
    drop(listener);
}

#[test]
fn denied_primary_unix_socket_falls_back_to_workspace_endpoint() {
    if effective_uid() == 0 {
        return;
    }
    let root = tempfile::TempDir::new().unwrap();
    ensure_daemon_dir(root.path()).unwrap();
    let denied_parent = root.path().join("denied-primary");
    std::fs::create_dir(&denied_parent).unwrap();
    std::fs::set_permissions(&denied_parent, std::fs::Permissions::from_mode(0o000)).unwrap();
    let primary = denied_parent.join("packet28d.sock");

    let listener = bind_preferred_daemon_listener(root.path(), &primary).unwrap();

    std::fs::set_permissions(&denied_parent, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        listener.endpoint(),
        workspace_socket_path(root.path()).display().to_string()
    );
    drop(listener);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn processor_failure_is_fatal_bounded_and_detached_work_retains_its_lease() {
    let state = daemon_test_state();
    let root = state.lock().unwrap().root.clone();
    let daemon_instance_lease =
        packet28_daemon_core::task_store_lease::acquire_daemon_instance_lease(&root).unwrap();
    // `daemon_test_state` has already completed startup recovery and its
    // persistence owner retains a shared lifecycle lease. Re-entering the
    // exclusive recovery phase here would wait on that owner forever.
    let task_store_lease =
        packet28_daemon_core::task_store_lease::acquire_daemon_task_store_lease(&root).unwrap();
    let shutdown = state.lock().unwrap().shutdown.clone();
    let blocking_pool = BlockingPool::with_lifecycle_leases(
        1,
        daemon_instance_lease.clone(),
        task_store_lease.clone(),
    );
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let (index_started_tx, index_started_rx) = tokio::sync::oneshot::channel();
    let (index_release_tx, index_release_rx) = std::sync::mpsc::sync_channel(1);
    let worker_pool = blocking_pool.clone();
    let mutation_state = state.clone();
    let blocking_worker = tokio::spawn(async move {
        worker_pool
            .run(move || {
                started_tx
                    .send(())
                    .map_err(|_| anyhow!("start probe closed"))?;
                release_rx
                    .recv()
                    .map_err(|_| anyhow!("release probe closed"))?;
                {
                    let mut guard = mutation_state.lock().map_err(lock_err)?;
                    guard.tasks.tasks.insert(
                        "task-during-shutdown".to_string(),
                        TaskRecord {
                            task_id: "task-during-shutdown".to_string(),
                            ..TaskRecord::default()
                        },
                    );
                    persist_state_for_test(&guard)?;
                }
                flush_persistence(&mutation_state)?;
                Ok(())
            })
            .await
    });
    started_rx.await.unwrap();
    let index_daemon_instance_lease = daemon_instance_lease.clone();
    let index_task_store_lease = task_store_lease.clone();
    let index_worker = tokio::task::spawn_blocking(move || {
        let _daemon_instance_lease = index_daemon_instance_lease;
        let _task_store_lease = index_task_store_lease;
        index_started_tx
            .send(())
            .map_err(|_| anyhow!("index start probe closed"))?;
        index_release_rx
            .recv()
            .map_err(|_| anyhow!("index release probe closed"))?;
        Ok(())
    });
    index_started_rx.await.unwrap();
    std::fs::write(ready_path(&root), b"ready\n").unwrap();

    let supervisor_started = Instant::now();
    let supervisor = tokio::spawn(supervise_daemon_tasks(
        state.clone(),
        shutdown.clone(),
        blocking_pool,
        Duration::from_millis(25),
        DaemonRuntimeTasks {
            transport: shutdown_waiter(shutdown.clone()),
            watch: tokio::spawn(async { Err(anyhow!("injected watch processor failure")) }),
            background: shutdown_waiter(shutdown.clone()),
            index: index_worker,
        },
    ));
    drop(task_store_lease);
    drop(daemon_instance_lease);
    let outcome = tokio::time::timeout(Duration::from_millis(500), supervisor)
        .await
        .expect("supervisor exceeded its shared shutdown deadline")
        .unwrap();
    assert!(
        supervisor_started.elapsed() < Duration::from_millis(500),
        "supervisor did not detach blocking work at the shared deadline"
    );
    let error = outcome
        .result
        .expect_err("injected processor failure was not fatal");
    assert!(
        format!("{error:#}").contains("injected watch processor failure"),
        "unexpected supervisor error: {error:#}"
    );
    assert!(shutdown.is_requested());
    assert!(state.lock().unwrap().shutting_down);
    assert!(
        !ready_path(&root).exists(),
        "fatal processor exit left daemon readiness published"
    );
    assert!(
        packet28_daemon_core::task_store_lease::try_acquire_task_store_retention_lease(&root)
            .unwrap()
            .is_none(),
        "exclusive retention acquired while detached blocking owners were still running"
    );
    assert!(
        packet28_daemon_core::task_store_lease::acquire_daemon_instance_lease(&root).is_err(),
        "a second daemon acquired the instance lease while detached owners were still running"
    );

    release_tx.send(()).unwrap();
    index_release_tx.send(()).unwrap();
    blocking_worker.await.unwrap().unwrap();
    shutdown_test_persistence(&state);
    assert!(
        load_task_registry(&root)
            .unwrap()
            .tasks
            .contains_key("task-during-shutdown"),
        "detached task-store mutation did not persist before releasing its lease"
    );
    let retention_deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if packet28_daemon_core::task_store_lease::try_acquire_task_store_retention_lease(&root)
            .unwrap()
            .is_some()
        {
            break;
        }
        assert!(
            Instant::now() < retention_deadline,
            "exclusive retention did not become available after detached owners exited"
        );
        tokio::task::yield_now().await;
    }
    assert!(
        packet28_daemon_core::task_store_lease::acquire_daemon_instance_lease(&root).is_ok(),
        "daemon instance lease remained held after detached owners exited"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn early_index_worker_failure_withdraws_readiness_and_stops_peer_owners() {
    let state = daemon_test_state();
    let root = state.lock().unwrap().root.clone();
    std::fs::write(ready_path(&root), b"ready\n").unwrap();
    let shutdown = state.lock().unwrap().shutdown.clone();

    let outcome = tokio::time::timeout(
        Duration::from_secs(1),
        supervise_daemon_tasks(
            state.clone(),
            shutdown.clone(),
            BlockingPool::new(1),
            Duration::from_millis(250),
            DaemonRuntimeTasks {
                transport: shutdown_waiter(shutdown.clone()),
                watch: shutdown_waiter(shutdown.clone()),
                background: shutdown_waiter(shutdown),
                index: tokio::task::spawn_blocking(|| Err(anyhow!("injected early index failure"))),
            },
        ),
    )
    .await
    .expect("early index failure did not stop daemon owners");
    let error = outcome
        .result
        .expect_err("early index failure was not fatal");

    assert!(format!("{error:#}").contains("injected early index failure"));
    assert!(state.lock().unwrap().shutting_down);
    assert!(!ready_path(&root).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unexpected_successful_index_exit_before_shutdown_remains_fatal() {
    let state = daemon_test_state();
    let root = state.lock().unwrap().root.clone();
    std::fs::write(ready_path(&root), b"ready\n").unwrap();
    let shutdown = state.lock().unwrap().shutdown.clone();

    let outcome = tokio::time::timeout(
        Duration::from_secs(1),
        supervise_daemon_tasks(
            state.clone(),
            shutdown.clone(),
            BlockingPool::new(1),
            Duration::from_millis(250),
            DaemonRuntimeTasks {
                transport: shutdown_waiter(shutdown.clone()),
                watch: shutdown_waiter(shutdown.clone()),
                background: shutdown_waiter(shutdown),
                index: tokio::task::spawn_blocking(|| Ok(())),
            },
        ),
    )
    .await
    .expect("unexpected successful index exit did not stop daemon owners");
    let error = outcome
        .result
        .expect_err("unexpected successful index exit was hidden");
    assert!(error
        .to_string()
        .contains("index worker exited before daemon shutdown"));
    assert!(!ready_path(&root).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepted_index_shutdown_worker_exit_is_clean_for_supervisor() {
    let state = daemon_test_state();
    let root = state.lock().unwrap().root.clone();
    std::fs::write(ready_path(&root), b"ready\n").unwrap();
    let shutdown = state.lock().unwrap().shutdown.clone();
    let (ingress, receiver) = IndexIngress::new();
    state.lock().unwrap().index_tx = ingress.clone();
    ingress
        .request_shutdown(|| shutdown.request())
        .expect("accept Stop shutdown");
    receiver.discard_until_shutdown();

    let outcome = tokio::time::timeout(
        Duration::from_secs(1),
        supervise_daemon_tasks(
            state.clone(),
            shutdown.clone(),
            BlockingPool::new(1),
            Duration::from_millis(250),
            DaemonRuntimeTasks {
                transport: shutdown_waiter(shutdown.clone()),
                watch: shutdown_waiter(shutdown.clone()),
                background: shutdown_waiter(shutdown),
                index: tokio::task::spawn_blocking(|| Ok(())),
            },
        ),
    )
    .await
    .expect("accepted shutdown did not join daemon owners");

    outcome
        .result
        .expect("supervisor rejected an already accepted index shutdown");
    assert!(!ready_path(&root).exists());
}

#[test]
fn production_cache_finalizer_is_bounded_and_retryable_when_root_lock_is_blocked() {
    let state = daemon_test_state();
    let (root, kernel) = {
        let guard = state.lock().unwrap();
        (guard.root.clone(), guard.kernel.clone())
    };
    let lock_path = root.join(".packet28/packet-cache-v3.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    FileExt::lock_exclusive(&lock).unwrap();
    kernel
        .execute(KernelRequest {
            target: "agenty.state.write".to_string(),
            reducer_input: json!({
                "task_id": "task-blocked-cache-shutdown",
                "event_id": "event-1",
                "occurred_at_unix": 1,
                "actor": "agent",
                "kind": "focus_set",
                "paths": ["src/cache.rs"],
                "data": {"type": "focus_set"}
            }),
            ..KernelRequest::default()
        })
        .unwrap();

    let started = Instant::now();
    let error = shutdown_persistent_kernels(&state, Duration::from_millis(25))
        .expect_err("blocked persistence root unexpectedly shut down");
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "production cache shutdown exceeded its lifecycle bound: {:?}",
        started.elapsed()
    );
    assert!(
        format!("{error:#}").contains("timed out"),
        "unexpected bounded shutdown error: {error:#}"
    );

    FileExt::unlock(&lock).unwrap();
    shutdown_persistent_kernels(&state, Duration::from_secs(2))
        .expect("cache persistence shutdown did not succeed on bounded retry");
}

#[test]
fn blocked_persistent_root_cannot_consume_later_roots_shutdown_budget() {
    let state = daemon_test_state();
    let (root, primary, registry) = {
        let guard = state.lock().unwrap();
        (
            guard.root.clone(),
            guard.kernel.clone(),
            guard.kernel_registry.clone(),
        )
    };
    let secondary_root = root.join("secondary-root");
    std::fs::create_dir_all(&secondary_root).unwrap();
    let secondary = registry.get(&secondary_root).unwrap();
    let lock_path = root.join(".packet28/packet-cache-v3.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    FileExt::lock_exclusive(&lock).unwrap();
    primary
        .execute(KernelRequest {
            target: "agenty.state.write".to_string(),
            reducer_input: json!({
                "task_id": "task-fair-cache-shutdown",
                "event_id": "event-1",
                "occurred_at_unix": 1,
                "actor": "agent",
                "kind": "focus_set",
                "paths": ["src/fair-cache.rs"],
                "data": {"type": "focus_set"}
            }),
            ..KernelRequest::default()
        })
        .unwrap();

    let started = Instant::now();
    let error = shutdown_persistent_kernels(&state, Duration::from_millis(200))
        .expect_err("blocked primary root unexpectedly shut down");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "multi-root cache shutdown exceeded its shared deadline: {:?}",
        started.elapsed()
    );
    assert!(format!("{error:#}").contains("timed out"));
    assert!(
        secondary
            .flush_cache_persistence(Duration::from_millis(25))
            .is_err(),
        "later persistent root was not shut down after the earlier root consumed its fair share"
    );

    FileExt::unlock(&lock).unwrap();
    shutdown_persistent_kernels(&state, Duration::from_secs(2))
        .expect("multi-root cache persistence shutdown did not succeed on bounded retry");
}

#[test]
fn slow_subscriber_is_closed_at_capacity_and_can_replay_exact_gap() {
    let state = daemon_test_state();
    insert_admitted_task_record(
        &state,
        TaskRecord {
            task_id: "task-slow".to_string(),
            ..TaskRecord::default()
        },
    );
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    state.lock().unwrap().subscribers.insert(
        "task-slow".to_string(),
        vec![crate::state::TaskSubscriber { id: 7, sender }],
    );

    emit_task_event(state.clone(), "task-slow", "first", json!({"ordinal": 1})).unwrap();
    emit_task_event(state.clone(), "task-slow", "second", json!({"ordinal": 2})).unwrap();

    assert!(!state.lock().unwrap().subscribers.contains_key("task-slow"));
    assert_eq!(receiver.try_recv().unwrap().seq, 1);
    assert!(matches!(
        receiver.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
    ));

    let root = state.lock().unwrap().root.clone();
    let replay = crate::server::select_replay_events(
        load_task_events(&root, "task-slow").unwrap(),
        0,
        Some(1),
    );
    assert_eq!(
        replay.iter().map(|frame| frame.seq).collect::<Vec<_>>(),
        vec![2]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tcp_stop_acknowledges_stops_accepting_and_joins_connection() {
    let _tcp_test_guard = TCP_TRANSPORT_TEST_LOCK.lock().await;
    let state = daemon_test_state();
    let listener = bind_tcp_listener("transport parity test").unwrap();
    let auth = listener.transport_auth().unwrap();
    let endpoint = listener.endpoint();
    let address = endpoint
        .strip_prefix("tcp://")
        .unwrap()
        .parse::<std::net::SocketAddr>()
        .unwrap();
    let (watch_tx, _watch_rx) = WatchIngress::new(8);
    let config = DaemonRuntimeConfig {
        shutdown_grace: Duration::from_secs(2),
        ..DaemonRuntimeConfig::default()
    };
    let server_state = state.clone();
    let server = tokio::spawn(run_transport(
        listener,
        server_state,
        watch_tx,
        BlockingPool::new(2),
        config,
    ));
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();

    assert!(matches!(
        exchange_value(&mut client, &auth).await,
        DaemonResponse::Ack { ref message } if message == "authenticated"
    ));
    assert!(matches!(
        exchange(&mut client, &DaemonRequest::Status).await,
        DaemonResponse::Status { .. }
    ));
    assert!(matches!(
        exchange(&mut client, &DaemonRequest::Stop).await,
        DaemonResponse::Ack { ref message } if message == "stopping"
    ));
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("TCP transport did not stop")
        .unwrap()
        .unwrap();

    drop(client);
    // Some kernels can complete one handshake that was already queued while
    // the listening descriptor closes. A live listener would keep accepting;
    // a closed one must refuse a subsequent bounded probe.
    for attempt in 1..=3 {
        match tokio::net::TcpStream::connect(address).await {
            Err(error) if error.kind() == ErrorKind::ConnectionRefused => return,
            Ok(reconnect) if attempt < 3 => drop(reconnect),
            outcome => {
                panic!(
                    "TCP endpoint still accepted connections after structured shutdown \
                     (attempt {attempt}): {outcome:?}"
                );
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tcp_transport_rejects_unauthenticated_stop_before_dispatch() {
    let _tcp_test_guard = TCP_TRANSPORT_TEST_LOCK.lock().await;
    let state = daemon_test_state();
    let listener = bind_tcp_listener("unauthenticated request test").unwrap();
    let auth = listener.transport_auth().unwrap();
    let address = listener
        .endpoint()
        .strip_prefix("tcp://")
        .unwrap()
        .parse::<std::net::SocketAddr>()
        .unwrap();
    let (watch_tx, _watch_rx) = WatchIngress::new(8);
    let server_state = state.clone();
    let server = tokio::spawn(run_transport(
        listener,
        server_state,
        watch_tx,
        BlockingPool::new(2),
        DaemonRuntimeConfig::default(),
    ));
    let mut unauthenticated = tokio::net::TcpStream::connect(address).await.unwrap();

    let rejected = exchange(&mut unauthenticated, &DaemonRequest::Stop).await;

    assert!(matches!(
        rejected,
        DaemonResponse::Error { ref message }
            if message == "daemon transport authentication failed"
    ));
    assert!(!state.lock().unwrap().shutdown.is_requested());

    let mut authorized = tokio::net::TcpStream::connect(address).await.unwrap();
    assert!(matches!(
        exchange_value(&mut authorized, &auth).await,
        DaemonResponse::Ack { ref message } if message == "authenticated"
    ));
    assert!(matches!(
        exchange(&mut authorized, &DaemonRequest::Stop).await,
        DaemonResponse::Ack { ref message } if message == "stopping"
    ));
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("TCP transport did not stop after authenticated cleanup")
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tcp_transport_rejects_wrong_capability_before_dispatch() {
    let _tcp_test_guard = TCP_TRANSPORT_TEST_LOCK.lock().await;
    let state = daemon_test_state();
    let listener = bind_tcp_listener("wrong capability test").unwrap();
    let auth = listener.transport_auth().unwrap();
    let address = listener
        .endpoint()
        .strip_prefix("tcp://")
        .unwrap()
        .parse::<std::net::SocketAddr>()
        .unwrap();
    let (watch_tx, _watch_rx) = WatchIngress::new(8);
    let server_state = state.clone();
    let server = tokio::spawn(run_transport(
        listener,
        server_state,
        watch_tx,
        BlockingPool::new(2),
        DaemonRuntimeConfig::default(),
    ));
    let wrong = DaemonTransportAuth::from_secret_bytes([0xff; DAEMON_TRANSPORT_SECRET_BYTES]);
    let mut unauthorized = tokio::net::TcpStream::connect(address).await.unwrap();

    let rejected = exchange_value(&mut unauthorized, &wrong).await;

    assert!(matches!(
        rejected,
        DaemonResponse::Error { ref message }
            if message == "daemon transport authentication failed"
    ));
    assert!(!state.lock().unwrap().shutdown.is_requested());

    let mut authorized = tokio::net::TcpStream::connect(address).await.unwrap();
    assert!(matches!(
        exchange_value(&mut authorized, &auth).await,
        DaemonResponse::Ack { ref message } if message == "authenticated"
    ));
    let _ = exchange(&mut authorized, &DaemonRequest::Stop).await;
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("TCP transport did not stop after wrong-capability test")
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unix_transport_matches_tcp_status_and_stop_semantics() {
    let state = daemon_test_state();
    let directory = tempfile::TempDir::new().unwrap();
    let socket = directory.path().join("packet28d.sock");
    let listener = DaemonListener::Unix {
        endpoint: socket.clone(),
        listener: UnixListener::bind(&socket).unwrap(),
        owner_uid: std::fs::symlink_metadata(&socket).unwrap().uid(),
    };
    let (watch_tx, _watch_rx) = WatchIngress::new(8);
    let config = DaemonRuntimeConfig {
        shutdown_grace: Duration::from_secs(2),
        ..DaemonRuntimeConfig::default()
    };
    let server_state = state.clone();
    let server = tokio::spawn(run_transport(
        listener,
        server_state,
        watch_tx,
        BlockingPool::new(2),
        config,
    ));
    let mut client = tokio::net::UnixStream::connect(&socket).await.unwrap();

    assert!(matches!(
        exchange(&mut client, &DaemonRequest::Status).await,
        DaemonResponse::Status { .. }
    ));
    assert!(matches!(
        exchange(&mut client, &DaemonRequest::Stop).await,
        DaemonResponse::Ack { ref message } if message == "stopping"
    ));
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("Unix transport did not stop")
        .unwrap()
        .unwrap();

    drop(client);
    std::fs::remove_file(&socket).unwrap();
    UnixListener::bind(&socket).expect("Unix endpoint remained in use after structured shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unix_transport_rejects_peer_that_does_not_match_daemon_owner() {
    let state = daemon_test_state();
    let directory = tempfile::TempDir::new().unwrap();
    let socket = directory.path().join("packet28d.sock");
    let unix_listener = UnixListener::bind(&socket).unwrap();
    let current_uid = std::fs::symlink_metadata(&socket).unwrap().uid();
    let listener = DaemonListener::Unix {
        endpoint: socket.clone(),
        listener: unix_listener,
        owner_uid: current_uid ^ 1,
    };
    let (watch_tx, _watch_rx) = WatchIngress::new(8);
    let server_state = state.clone();
    let server = tokio::spawn(run_transport(
        listener,
        server_state,
        watch_tx,
        BlockingPool::new(2),
        DaemonRuntimeConfig::default(),
    ));
    let mut client = tokio::net::UnixStream::connect(&socket).await.unwrap();
    let mut encoded = Vec::new();
    write_frame(&mut encoded, &DaemonRequest::Stop).unwrap();
    let rejected = match client.write_all(&encoded).await {
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::ConnectionReset | ErrorKind::BrokenPipe | ErrorKind::NotConnected
            ) =>
        {
            true
        }
        Err(error) => panic!("unexpected Unix peer rejection write error: {error}"),
        Ok(()) => {
            let mut byte = [0_u8; 1];
            let read = tokio::time::timeout(Duration::from_secs(1), client.read(&mut byte))
                .await
                .expect("owner-mismatched Unix peer was not rejected");
            match read {
                Ok(0) => true,
                Err(error) => matches!(
                    error.kind(),
                    ErrorKind::ConnectionReset | ErrorKind::BrokenPipe | ErrorKind::NotConnected
                ),
                Ok(_) => false,
            }
        }
    };
    assert!(rejected, "owner-mismatched Unix peer received data");
    assert!(!state.lock().unwrap().shutdown.is_requested());

    state.lock().unwrap().shutdown.request();
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("Unix transport did not stop after peer-owner test")
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unauthenticated_tcp_peer_does_not_consume_authenticated_connection_budget() {
    let _tcp_test_guard = TCP_TRANSPORT_TEST_LOCK.lock().await;
    let state = daemon_test_state();
    let listener = bind_tcp_listener("pre-authentication isolation test").unwrap();
    let auth = listener.transport_auth().unwrap();
    let endpoint = listener.endpoint();
    let address = endpoint
        .strip_prefix("tcp://")
        .unwrap()
        .parse::<std::net::SocketAddr>()
        .unwrap();
    let (watch_tx, _watch_rx) = WatchIngress::new(8);
    let config = DaemonRuntimeConfig {
        max_connections: 1,
        max_pending_tcp_authentications: 2,
        frame_header_timeout: Duration::from_secs(5),
        transport_auth_timeout: Duration::from_secs(2),
        shutdown_grace: Duration::from_secs(2),
        ..DaemonRuntimeConfig::default()
    };
    let server_state = state.clone();
    let server = tokio::spawn(run_transport(
        listener,
        server_state,
        watch_tx,
        BlockingPool::new(1),
        config,
    ));

    let mut stalled = tokio::net::TcpStream::connect(address).await.unwrap();
    stalled.write_all(&[0_u8]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut authorized = tokio::net::TcpStream::connect(address).await.unwrap();
    assert!(matches!(
        exchange_value(&mut authorized, &auth).await,
        DaemonResponse::Ack { ref message } if message == "authenticated"
    ));
    assert!(matches!(
        exchange(&mut authorized, &DaemonRequest::Status).await,
        DaemonResponse::Status { .. }
    ));
    assert!(matches!(
        exchange(&mut authorized, &DaemonRequest::Stop).await,
        DaemonResponse::Ack { ref message } if message == "stopping"
    ));

    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("transport did not join with a stalled unauthenticated peer")
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_tcp_authentication_cap_is_bounded_and_times_out() {
    let _tcp_test_guard = TCP_TRANSPORT_TEST_LOCK.lock().await;
    let state = daemon_test_state();
    let listener = bind_tcp_listener("pending authentication cap test").unwrap();
    let auth = listener.transport_auth().unwrap();
    let address = listener
        .endpoint()
        .strip_prefix("tcp://")
        .unwrap()
        .parse::<std::net::SocketAddr>()
        .unwrap();
    let (watch_tx, _watch_rx) = WatchIngress::new(8);
    let config = DaemonRuntimeConfig {
        max_connections: 1,
        max_pending_tcp_authentications: 1,
        frame_header_timeout: Duration::from_secs(5),
        transport_auth_timeout: Duration::from_millis(500),
        shutdown_grace: Duration::from_secs(2),
        ..DaemonRuntimeConfig::default()
    };
    let server_state = state.clone();
    let server = tokio::spawn(run_transport(
        listener,
        server_state,
        watch_tx,
        BlockingPool::new(1),
        config,
    ));

    let mut stalled = tokio::net::TcpStream::connect(address).await.unwrap();
    stalled.write_all(&[0_u8]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut excess = tokio::net::TcpStream::connect(address).await.unwrap();
    let mut encoded = Vec::new();
    write_frame(&mut encoded, &auth).unwrap();
    let rejected = match excess.write_all(&encoded).await {
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::ConnectionReset | ErrorKind::BrokenPipe
            ) =>
        {
            true
        }
        Err(error) => panic!("unexpected excess authentication write error: {error}"),
        Ok(()) => {
            let mut byte = [0_u8; 1];
            let read = tokio::time::timeout(Duration::from_secs(1), excess.read(&mut byte))
                .await
                .expect("excess pending authentication was neither rejected nor serviced");
            match read {
                Ok(0) => true,
                Err(error) => matches!(
                    error.kind(),
                    ErrorKind::ConnectionReset | ErrorKind::BrokenPipe
                ),
                Ok(_) => false,
            }
        }
    };
    assert!(
        rejected,
        "pending authentication cap admitted an excess peer"
    );

    tokio::time::sleep(Duration::from_millis(550)).await;
    let mut authorized = tokio::net::TcpStream::connect(address).await.unwrap();
    assert!(matches!(
        exchange_value(&mut authorized, &auth).await,
        DaemonResponse::Ack { ref message } if message == "authenticated"
    ));
    assert!(matches!(
        exchange(&mut authorized, &DaemonRequest::Stop).await,
        DaemonResponse::Ack { ref message } if message == "stopping"
    ));
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("transport did not recover after pending authentication timeout")
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection_cap_rejects_excess_authenticated_peer() {
    let _tcp_test_guard = TCP_TRANSPORT_TEST_LOCK.lock().await;
    let state = daemon_test_state();
    let listener = bind_tcp_listener("authenticated connection cap test").unwrap();
    let auth = listener.transport_auth().unwrap();
    let address = listener
        .endpoint()
        .strip_prefix("tcp://")
        .unwrap()
        .parse::<std::net::SocketAddr>()
        .unwrap();
    let (watch_tx, _watch_rx) = WatchIngress::new(8);
    let config = DaemonRuntimeConfig {
        max_connections: 1,
        shutdown_grace: Duration::from_secs(2),
        ..DaemonRuntimeConfig::default()
    };
    let server_state = state.clone();
    let server = tokio::spawn(run_transport(
        listener,
        server_state,
        watch_tx,
        BlockingPool::new(1),
        config,
    ));

    let mut first = tokio::net::TcpStream::connect(address).await.unwrap();
    assert!(matches!(
        exchange_value(&mut first, &auth).await,
        DaemonResponse::Ack { ref message } if message == "authenticated"
    ));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut excess = tokio::net::TcpStream::connect(address).await.unwrap();
    assert!(matches!(
        exchange_value(&mut excess, &auth).await,
        DaemonResponse::Ack { ref message } if message == "authenticated"
    ));
    let mut encoded = Vec::new();
    write_frame(&mut encoded, &DaemonRequest::Status).unwrap();
    let rejected = match excess.write_all(&encoded).await {
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::ConnectionReset | ErrorKind::BrokenPipe
            ) =>
        {
            true
        }
        Err(error) => panic!("unexpected capped connection write error: {error}"),
        Ok(()) => {
            let mut byte = [0_u8; 1];
            let read = tokio::time::timeout(Duration::from_secs(1), excess.read(&mut byte))
                .await
                .expect("excess authenticated connection was neither rejected nor serviced");
            match read {
                Ok(0) => true,
                Err(error) => matches!(
                    error.kind(),
                    ErrorKind::ConnectionReset | ErrorKind::BrokenPipe
                ),
                Ok(_) => false,
            }
        }
    };
    assert!(
        rejected,
        "authenticated connection cap admitted an excess peer"
    );

    assert!(matches!(
        exchange(&mut first, &DaemonRequest::Stop).await,
        DaemonResponse::Ack { ref message } if message == "stopping"
    ));
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("authenticated connection cap transport did not join")
        .unwrap()
        .unwrap();
}
