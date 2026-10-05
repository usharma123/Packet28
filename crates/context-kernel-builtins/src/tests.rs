use super::*;
use context_memory_core::{CachePacket, CachePersistence, NoopDeltaReuseHooks};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, OnceLock};
use tempfile::tempdir;

mod cache;
mod sequence;

#[test]
fn v1_registry_contains_the_exact_supported_catalog() {
    let expected = vec![
        "agenty.state.snapshot",
        "agenty.state.write",
        "buildy.reduce",
        "contextq.assemble",
        "contextq.correlate",
        "contextq.manage",
        "diffy.analyze",
        "governed.assemble",
        "guardy.check",
        "mapy.query",
        "mapy.repo",
        "packet28.broker_memory.write",
        "packet28.instruction.summarize",
        "proxy.run",
        "stacky.slice",
        "testy.impact",
    ];

    assert!(Kernel::new().reducer_names().is_empty());
    assert_eq!(Kernel::with_v1_reducers().reducer_names(), expected);
}

#[test]
fn every_v1_target_routes_to_its_adapter() {
    let kernel = Kernel::with_v1_reducers();

    for target in kernel.reducer_names() {
        let result = kernel.execute(KernelRequest {
            target: target.clone(),
            policy_context: json!({"disable_cache": true}),
            reducer_input: Value::String("routing-probe".to_string()),
            ..KernelRequest::default()
        });
        assert!(
            !matches!(result, Err(KernelError::UnknownTarget { .. })),
            "registered target {target} did not route to an adapter"
        );
    }
}

fn fixture(rel: &str) -> String {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    workspace
        .join("tests")
        .join("fixtures")
        .join(rel)
        .to_string_lossy()
        .to_string()
}

fn git_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed with {status}");
}

fn setup_diff_repo(dir: &Path) {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/alpha.rs"), "pub fn alpha() -> i32 { 1 }\n").unwrap();
    std::fs::write(dir.join("src/beta.rs"), "pub fn beta() -> i32 { 2 }\n").unwrap();

    git(dir, &["init"]);
    git(dir, &["add", "src/alpha.rs", "src/beta.rs"]);
    git(
        dir,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            "init",
        ],
    );

    std::fs::write(dir.join("src/alpha.rs"), "pub fn alpha() -> i32 { 3 }\n").unwrap();
    git(dir, &["add", "src/alpha.rs"]);
    git(
        dir,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            "change alpha",
        ],
    );
}

fn write_policy_file(path: &Path, tools: &[&str], reducers: &[&str]) {
    let tools_yaml = if tools.is_empty() {
        "[]".to_string()
    } else {
        format!(
            "[{}]",
            tools
                .iter()
                .map(|tool| format!("\"{tool}\""))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let reducers_yaml = if reducers.is_empty() {
        "[]".to_string()
    } else {
        format!(
            "[{}]",
            reducers
                .iter()
                .map(|reducer| format!("\"{reducer}\""))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };

    std::fs::write(
        path,
        format!(
            r#"
version: 1
policy:
  allowed_tools: {tools_yaml}
  allowed_reducers: {reducers_yaml}
  paths:
    include: ["src/**"]
    exclude: []
  budgets:
    token_cap: 2000
    runtime_ms_cap: 2000
  redaction:
    forbidden_patterns: []
"#
        ),
    )
    .unwrap();
}

#[test]
fn errors_for_unknown_target() {
    let kernel = Kernel::new();
    let err = kernel
        .execute(KernelRequest {
            target: "missing.reducer".to_string(),
            ..KernelRequest::default()
        })
        .unwrap_err();

    match err {
        KernelError::UnknownTarget { target, registered } => {
            assert_eq!(target, "missing.reducer");
            assert!(registered.is_empty());
        }
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn enforces_input_token_budget() {
    let mut kernel = Kernel::new();
    kernel.register_reducer("noop", |_ctx, _packets| Ok(ReducerResult::default()));

    let packet = KernelPacket {
        body: json!({"text": "this payload should exceed tiny token budget"}),
        ..KernelPacket::default()
    };

    let err = kernel
        .execute(KernelRequest {
            target: "noop".to_string(),
            input_packets: vec![packet],
            budget: ExecutionBudget {
                token_cap: Some(1),
                byte_cap: None,
                runtime_ms_cap: None,
            },
            ..KernelRequest::default()
        })
        .unwrap_err();

    assert!(matches!(
        err,
        KernelError::BudgetExceeded {
            stage: BudgetStage::Input,
            metric: BudgetMetric::Tokens,
            ..
        }
    ));
}

#[test]
fn contextq_reducer_assembles_packets() {
    let kernel = Kernel::with_v1_reducers();
    let packet_a = KernelPacket::from_value(
        json!({
            "packet_id": "diffy",
            "tool": "diffy",
            "reducer": "reduce",
            "sections": [{
                "title": "Diff",
                "body": "critical regression",
                "refs": [{"kind": "file", "value": "src/lib.rs"}],
                "relevance": 0.9
            }]
        }),
        None,
    );
    let packet_b = KernelPacket::from_value(
        json!({
            "packet_id": "impact",
            "tool": "testy",
            "reducer": "reduce",
            "sections": [{
                "title": "Impact",
                "body": "selected tests",
                "refs": [{"kind": "symbol", "value": "foo::bar"}],
                "relevance": 0.8
            }]
        }),
        None,
    );

    let response = kernel
        .execute(KernelRequest {
            target: "contextq.assemble".to_string(),
            input_packets: vec![packet_a, packet_b],
            budget: ExecutionBudget {
                token_cap: Some(1200),
                byte_cap: Some(24_000),
                runtime_ms_cap: Some(1_000),
            },
            ..KernelRequest::default()
        })
        .unwrap();

    assert_eq!(response.output_packets.len(), 1);
    let kind = response.output_packets[0]
        .body
        .get("kind")
        .and_then(Value::as_str)
        .unwrap();
    assert_eq!(kind, "context_assemble");
}

#[test]
fn policy_enforcement_rejects_disallowed_packet_before_contextq() {
    let dir = tempdir().unwrap();
    let config_path = dir.path().join("context.yaml");
    write_policy_file(&config_path, &["contextq"], &["assemble"]);

    let kernel = Kernel::with_v1_reducers();
    let packet = KernelPacket::from_value(
        json!({
            "tool": "diffy",
            "reducer": "analyze",
            "paths": ["src/lib.rs"],
            "payload": {"gate_result": {"passed": true}}
        }),
        None,
    );

    let err = kernel
        .execute(KernelRequest {
            target: "contextq.assemble".to_string(),
            input_packets: vec![packet],
            policy_context: json!({
                "config_path": config_path.to_string_lossy().to_string()
            }),
            ..KernelRequest::default()
        })
        .unwrap_err();

    assert!(matches!(err, KernelError::PolicyViolation { .. }));
}

#[test]
fn governed_assemble_surfaces_governance_audit() {
    let dir = tempdir().unwrap();
    let config_path = dir.path().join("context.yaml");
    write_policy_file(
        &config_path,
        &["diffy", "contextq"],
        &["analyze", "assemble", "contextq.assemble"],
    );

    let kernel = Kernel::with_v1_reducers();
    let packet = KernelPacket::from_value(
        json!({
            "packet_id": "diffy-analyze-v1",
            "tool": "diffy",
            "reducer": "analyze",
            "paths": ["src/lib.rs"],
            "payload": {"summary": "ok"},
            "sections": [{
                "title": "Diff Gate Summary",
                "body": "passed: true",
                "refs": [{"kind":"file","value":"src/lib.rs"}],
                "relevance": 1.0
            }]
        }),
        None,
    );

    let response = kernel
        .execute(KernelRequest {
            target: "governed.assemble".to_string(),
            input_packets: vec![packet],
            policy_context: json!({
                "config_path": config_path.to_string_lossy().to_string()
            }),
            budget: ExecutionBudget {
                token_cap: Some(1200),
                byte_cap: Some(24_000),
                runtime_ms_cap: Some(1_000),
            },
            ..KernelRequest::default()
        })
        .unwrap();

    assert_eq!(response.output_packets.len(), 1);
    assert!(response.audit.governance.enabled);
    assert!(response
        .audit
        .governance
        .reducer_execution
        .as_ref()
        .is_some_and(|audit| audit.allowed));
    assert_eq!(response.audit.governance.input_audits.len(), 1);
    assert_eq!(response.audit.governance.output_audits.len(), 1);
    assert!(response.audit.governance.input_audits[0].passed);
    assert!(response.audit.governance.output_audits[0].passed);
}

#[test]
fn contextq_assemble_exposes_budget_trim_metadata() {
    let kernel = Kernel::with_v1_reducers();
    let packet = KernelPacket::from_value(
        json!({
            "packet_id": "large-packet",
            "tool": "diffy",
            "reducer": "analyze",
            "sections": [{
                "title": "Large section",
                "body": "X".repeat(8_000),
                "refs": [{"kind":"file","value":"src/lib.rs"}],
                "relevance": 1.0
            }]
        }),
        None,
    );
    let mut packet = packet;
    packet.token_usage = Some(1);

    let response = kernel
        .execute(KernelRequest {
            target: "contextq.assemble".to_string(),
            input_packets: vec![packet],
            budget: ExecutionBudget {
                token_cap: Some(1300),
                byte_cap: Some(200_000),
                runtime_ms_cap: None,
            },
            ..KernelRequest::default()
        })
        .unwrap();

    let truncated = response
        .metadata
        .get("budget_trim")
        .and_then(|trim| trim.get("truncated"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    assert!(truncated);
    assert!(response
        .metadata
        .get("budget_trim")
        .and_then(|trim| trim.get("sections_dropped"))
        .and_then(Value::as_u64)
        .is_some());
    assert!(response
        .metadata
        .get("budget_trim")
        .and_then(|trim| trim.get("refs_dropped"))
        .and_then(Value::as_u64)
        .is_some());
}

#[test]
fn policy_enforcement_rejects_disallowed_reducer_execution() {
    let dir = tempdir().unwrap();
    let config_path = dir.path().join("context.yaml");
    write_policy_file(&config_path, &[], &["assemble"]);

    let mut kernel = Kernel::new();
    kernel.register_reducer("custom.run", |_ctx, _packets| Ok(ReducerResult::default()));

    let err = kernel
        .execute(KernelRequest {
            target: "custom.run".to_string(),
            policy_context: json!({
                "config_path": config_path.to_string_lossy().to_string()
            }),
            ..KernelRequest::default()
        })
        .unwrap_err();

    match err {
        KernelError::PolicyViolation { detail, .. } => {
            assert!(detail.contains("reducer execution 'custom.run'"));
        }
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn guardy_reducer_runs_policy_check() {
    let dir = tempdir().unwrap();
    let config_path = dir.path().join("context.yaml");
    std::fs::write(
        &config_path,
        r#"
version: 1
policy:
  allowed_tools: ["covy"]
  allowed_reducers: ["merge"]
  paths:
    include: ["src/**"]
    exclude: []
  budgets:
    token_cap: 200
    runtime_ms_cap: 1000
  redaction:
    forbidden_patterns: []
"#,
    )
    .unwrap();

    let kernel = Kernel::with_v1_reducers();
    let packet = KernelPacket::from_value(
        json!({
            "tool": "covy",
            "reducer": "merge",
            "paths": ["src/lib.rs"],
            "token_usage": 50,
            "runtime_ms": 10,
            "payload": {"message": "ok"}
        }),
        None,
    );

    let response = kernel
        .execute(KernelRequest {
            target: "guardy.check".to_string(),
            input_packets: vec![packet],
            policy_context: json!({
                "config_path": config_path.to_string_lossy().to_string()
            }),
            ..KernelRequest::default()
        })
        .unwrap();

    let passed = response.output_packets[0]
        .body
        .get("payload")
        .and_then(|payload| payload.get("passed"))
        .and_then(Value::as_bool)
        .unwrap();
    assert!(passed);
}

#[test]
fn guardy_reducer_scans_wrapped_packet_payloads() {
    let dir = tempdir().unwrap();
    let config_path = dir.path().join("context.yaml");
    std::fs::write(
        &config_path,
        r#"
version: 1
policy:
  paths:
    include: ["**"]
    exclude: []
  redaction:
    forbidden_patterns: ["secret123"]
"#,
    )
    .unwrap();

    let kernel = Kernel::with_v1_reducers();
    let packet = KernelPacket::from_value(
        json!({
            "schema_version": "suite.packet.v1",
            "packet_type": "suite.proxy.run.v1",
            "packet": {
                "tool": "proxy",
                "payload": {
                    "highlights": ["my_password_is_secret123"]
                }
            }
        }),
        None,
    );

    let response = kernel
        .execute(KernelRequest {
            target: "guardy.check".to_string(),
            input_packets: vec![packet],
            policy_context: json!({
                "config_path": config_path.to_string_lossy().to_string()
            }),
            ..KernelRequest::default()
        })
        .unwrap();

    let passed = response.output_packets[0]
        .body
        .get("payload")
        .and_then(|payload| payload.get("passed"))
        .and_then(Value::as_bool)
        .unwrap();
    assert!(!passed);
}

#[test]
fn agenty_state_write_rejects_invalid_event_shape() {
    let kernel = Kernel::with_v1_reducers();
    let err = kernel
        .execute(KernelRequest {
            target: "agenty.state.write".to_string(),
            reducer_input: json!({
                "task_id": "task-a",
                "event_id": "evt-1",
                "occurred_at_unix": 1,
                "actor": "agent",
                "kind": "focus_set",
                "data": {"type": "focus_set"}
            }),
            ..KernelRequest::default()
        })
        .unwrap_err();

    assert!(matches!(err, KernelError::InvalidRequest { .. }));
}

#[test]
fn agenty_state_snapshot_derives_current_task_state() {
    let dir = tempdir().unwrap();
    let kernel =
        Kernel::with_v1_reducers_and_persistence(PersistConfig::new(dir.path().to_path_buf()));
    let events = [
        json!({
            "task_id": "task-a",
            "event_id": "evt-1",
            "occurred_at_unix": 1,
            "actor": "agent",
            "kind": "focus_set",
            "paths": ["src/time/StopWatch.java"],
            "symbols": ["split"],
            "data": {"type": "focus_set"}
        }),
        json!({
            "task_id": "task-a",
            "event_id": "evt-2",
            "occurred_at_unix": 2,
            "actor": "agent",
            "kind": "decision_added",
            "paths": ["src/time/StopWatch.java"],
            "symbols": ["split"],
            "data": {
                "type": "decision_added",
                "decision_id": "d1",
                "text": "Bug is in split()",
                "supersedes": null,
                "artifact_id": "artifact-split"
            }
        }),
        json!({
            "task_id": "task-a",
            "event_id": "evt-3",
            "occurred_at_unix": 3,
            "actor": "agent",
            "kind": "intention_recorded",
            "paths": ["src/time/StopWatch.java"],
            "symbols": ["split"],
            "data": {
                "type": "intention_recorded",
                "text": "Inspect split() before patching it",
                "note": "Need a fresh handoff breadcrumb",
                "step_id": "investigating",
                "question_id": "q1"
            }
        }),
        json!({
            "task_id": "task-a",
            "event_id": "evt-4",
            "occurred_at_unix": 4,
            "actor": "agent",
            "kind": "question_opened",
            "data": {
                "type": "question_opened",
                "question_id": "q1",
                "text": "Does DateUtils call split()?"
            }
        }),
        json!({
            "task_id": "task-a",
            "event_id": "evt-5",
            "occurred_at_unix": 5,
            "actor": "agent",
            "kind": "question_resolved",
            "data": {
                "type": "question_resolved",
                "question_id": "q1"
            }
        }),
        json!({
            "task_id": "task-a",
            "event_id": "evt-6",
            "occurred_at_unix": 6,
            "actor": "agent",
            "kind": "step_completed",
            "data": {
                "type": "step_completed",
                "step_id": "read_diff"
            }
        }),
        json!({
            "task_id": "task-a",
            "event_id": "z-add",
            "occurred_at_unix": 7,
            "actor": "agent",
            "kind": "decision_added",
            "data": {
                "type": "decision_added",
                "decision_id": "d2",
                "text": "Transient hypothesis",
                "supersedes": null
            }
        }),
        json!({
            "task_id": "task-a",
            "event_id": "a-supersede",
            "occurred_at_unix": 7,
            "actor": "agent",
            "kind": "decision_superseded",
            "data": {
                "type": "decision_superseded",
                "decision_id": "d2"
            }
        }),
    ];

    for event in events {
        kernel
            .execute(KernelRequest {
                target: "agenty.state.write".to_string(),
                reducer_input: event,
                ..KernelRequest::default()
            })
            .unwrap();
    }

    let response = kernel
        .execute(KernelRequest {
            target: "agenty.state.snapshot".to_string(),
            reducer_input: json!({
                "task_id": "task-a"
            }),
            policy_context: json!({
                "disable_cache": true
            }),
            ..KernelRequest::default()
        })
        .unwrap();
    let packet = response.output_packets.first().unwrap();
    let envelope: suite_packet_core::EnvelopeV1<suite_packet_core::AgentSnapshotPayload> =
        serde_json::from_value(packet.body.clone()).unwrap();

    assert_eq!(envelope.payload.task_id, "task-a");
    assert_eq!(envelope.payload.event_count, 8);
    assert_eq!(
        envelope.payload.focus_paths,
        vec!["src/time/StopWatch.java".to_string()]
    );
    assert_eq!(envelope.payload.focus_symbols, vec!["split".to_string()]);
    assert_eq!(
        envelope.payload.completed_steps,
        vec!["read_diff".to_string()]
    );
    assert!(envelope.payload.open_questions.is_empty());
    assert_eq!(envelope.payload.active_decisions.len(), 1);
    assert_eq!(envelope.payload.active_decisions[0].id, "d1");
    assert_eq!(
        envelope.payload.active_decisions[0].related_paths,
        vec!["src/time/StopWatch.java".to_string()]
    );
    assert_eq!(
        envelope.payload.active_decisions[0].related_symbols,
        vec!["split".to_string()]
    );
    assert_eq!(
        envelope.payload.active_decisions[0].related_artifact_ids,
        vec!["artifact-split".to_string()]
    );
    assert_eq!(
        envelope
            .payload
            .latest_intention
            .as_ref()
            .map(|intention| intention.text.as_str()),
        Some("Inspect split() before patching it")
    );
    assert_eq!(
        envelope
            .payload
            .latest_intention
            .as_ref()
            .and_then(|intention| intention.step_id.as_deref()),
        Some("investigating")
    );
}

fn cached_agent_entries(root: &Path) -> Vec<context_memory_core::PacketCacheEntry> {
    CachePersistence::open(PersistConfig::new(root.to_path_buf()))
        .unwrap()
        .shared_cache()
        .lock()
        .unwrap()
        .entries()
}

fn agent_snapshot(kernel: &Kernel, task_id: &str) -> suite_packet_core::AgentSnapshotPayload {
    let response = kernel
        .execute(KernelRequest {
            target: "agenty.state.snapshot".to_string(),
            reducer_input: json!({ "task_id": task_id }),
            ..KernelRequest::default()
        })
        .unwrap();
    let envelope: suite_packet_core::EnvelopeV1<suite_packet_core::AgentSnapshotPayload> =
        serde_json::from_value(response.output_packets[0].body.clone()).unwrap();
    envelope.payload
}

fn write_agent_event(kernel: &Kernel, event: Value) {
    kernel
        .execute(KernelRequest {
            target: "agenty.state.write".to_string(),
            reducer_input: event,
            ..KernelRequest::default()
        })
        .unwrap();
}

fn recovered_from(task_id: &str, predecessor_task_id: &str) -> Value {
    json!({
        "task_id": task_id,
        "event_id": format!("history-recovery:{predecessor_task_id}"),
        "occurred_at_unix": 50,
        "actor": "packet28d",
        "kind": "recovered_from",
        "data": {"type": "recovered_from", "predecessor_task_id": predecessor_task_id}
    })
}

fn completed_invocation(task_id: &str, sequence: u64, occurred_at_unix: u64) -> Value {
    json!({
        "task_id": task_id,
        "event_id": format!("{task_id}-invocation-{sequence}"),
        "occurred_at_unix": occurred_at_unix,
        "actor": "agent",
        "kind": "tool_invocation_completed",
        "data": {
            "type": "tool_invocation_completed",
            "invocation_id": format!("tool-invocation-{sequence}"),
            "sequence": sequence,
            "tool_name": "packet28.search",
            "operation_kind": "search",
            "artifact_id": format!("tool-invocation-{sequence}-result")
        }
    })
}

#[test]
fn agenty_state_snapshot_continues_recovery_predecessors_without_copying_them() {
    let dir = tempdir().unwrap();
    let kernel =
        Kernel::with_v1_reducers_and_persistence(PersistConfig::new(dir.path().to_path_buf()));
    for event in [
        json!({
            "task_id": "old",
            "event_id": "evt-decision",
            "occurred_at_unix": 1,
            "actor": "agent",
            "kind": "decision_added",
            "paths": ["src/auth.rs"],
            "data": {"type": "decision_added", "decision_id": "d1", "text": "Use tokens"}
        }),
        json!({
            "task_id": "old",
            "event_id": "evt-question",
            "occurred_at_unix": 2,
            "actor": "agent",
            "kind": "question_opened",
            "data": {"type": "question_opened", "question_id": "q1", "text": "Which store?"}
        }),
        completed_invocation("old", 40, 3),
        json!({
            "task_id": "unrelated",
            "event_id": "evt-other",
            "occurred_at_unix": 3,
            "actor": "agent",
            "kind": "decision_added",
            "data": {"type": "decision_added", "decision_id": "d9", "text": "Other task"}
        }),
        recovered_from("mid", "old"),
        completed_invocation("mid", 1, 60),
        json!({
            "task_id": "mid",
            "event_id": "evt-resolved",
            "occurred_at_unix": 0,
            "actor": "agent",
            "kind": "question_resolved",
            "data": {"type": "question_resolved", "question_id": "q1"}
        }),
        recovered_from("new", "mid"),
        completed_invocation("new", 1, 70),
    ] {
        write_agent_event(&kernel, event);
    }
    // Repeating the startup link is a cache hit, not a second link.
    write_agent_event(&kernel, recovered_from("new", "mid"));

    let new = agent_snapshot(&kernel, "new");
    assert_eq!(new.task_id, "new");
    assert_eq!(new.event_count, 6);
    assert_eq!(new.active_decisions.len(), 1);
    assert_eq!(new.active_decisions[0].id, "d1");
    assert!(new.open_questions.is_empty());
    // Sequences restart per session: the predecessors' invocations order
    // first even though "old" used sequence 40.
    assert_eq!(
        new.recent_tool_invocations
            .iter()
            .map(|invocation| (invocation.invocation_id.as_str(), invocation.sequence))
            .collect::<Vec<_>>(),
        vec![
            ("tool-invocation-40", 40),
            ("tool-invocation-1", 1),
            ("tool-invocation-1", 1),
        ]
    );
    assert_eq!(
        new.recent_tool_invocations
            .iter()
            .map(|invocation| invocation.owner_task_id.as_deref().unwrap())
            .collect::<Vec<_>>(),
        vec!["old", "mid", "new"]
    );
    // Same-named handles from different owners stay distinct references.
    assert_eq!(
        new.evidence_artifact_ids,
        vec![
            "tool-invocation-1-result".to_string(),
            "tool-invocation-40-result".to_string()
        ]
    );

    // Predecessors keep their own state; the cached events are not rewritten.
    let old = agent_snapshot(&kernel, "old");
    assert_eq!(old.event_count, 3);
    assert_eq!(old.open_questions.len(), 1);
    let mid = agent_snapshot(&kernel, "mid");
    assert_eq!(mid.event_count, 5);
    let events = cached_agent_entries(dir.path())
        .iter()
        .flat_map(crate::agenty_runtime::extract_agent_state_events)
        .filter(|event| event.event_id == "old-invocation-40")
        .map(|event| event.task_id)
        .collect::<Vec<_>>();
    assert_eq!(events, vec!["old".to_string()]);
}

#[test]
fn recovery_link_startup_does_not_copy_history_larger_than_one_cache_record() {
    let dir = tempdir().unwrap();
    let persistence = PersistConfig::new(dir.path().to_path_buf());
    {
        let kernel = Kernel::with_v1_reducers_and_persistence(persistence.clone());
        // Every individual event is valid, while their aggregate exceeds 64 MiB.
        let text = "x".repeat(1024 * 1024);
        for index in 0..65 {
            write_agent_event(
                &kernel,
                json!({
                    "task_id":"old", "event_id":format!("large-{index}"),
                    "occurred_at_unix":index + 1, "actor":"agent", "kind":"decision_added",
                    "data":{"type":"decision_added", "decision_id":format!("d-{index}"), "text":text}
                }),
            );
        }
        write_agent_event(&kernel, recovered_from("mid", "old"));
        kernel
            .flush_cache_persistence(Duration::from_secs(30))
            .unwrap();
    }
    // A restart between linked generations requires only another small record.
    let kernel = Kernel::with_v1_reducers_and_persistence(persistence);
    write_agent_event(&kernel, recovered_from("mid", "old"));
    write_agent_event(&kernel, recovered_from("new", "mid"));
    let entries = cached_agent_entries(dir.path());
    let events = entries
        .iter()
        .flat_map(crate::agenty_runtime::extract_agent_state_events)
        .collect::<Vec<_>>();
    assert_eq!(
        events.iter().filter(|event| event.task_id == "old").count(),
        65
    );
    assert_eq!(
        events.iter().filter(|event| event.task_id == "mid").count(),
        1
    );
    assert_eq!(
        events.iter().filter(|event| event.task_id == "new").count(),
        1
    );
}

#[test]
fn recovery_lineage_never_merges_ambiguous_or_cyclic_links() {
    let link = |task_id: &str, predecessor: &str| suite_packet_core::AgentStateEventPayload {
        task_id: task_id.to_string(),
        event_id: format!("{task_id}-{predecessor}"),
        actor: "packet28d".to_string(),
        kind: suite_packet_core::AgentStateEventKind::RecoveredFrom,
        data: suite_packet_core::AgentStateEventData::RecoveredFrom {
            predecessor_task_id: predecessor.to_string(),
        },
        ..suite_packet_core::AgentStateEventPayload::default()
    };
    let lineage = crate::agenty_runtime::recovery_lineage;
    assert_eq!(lineage(&[], "task"), vec!["task".to_string()]);
    assert_eq!(
        lineage(&[link("c", "b"), link("b", "a")], "c"),
        vec!["a".to_string(), "b".to_string(), "c".to_string()]
    );
    assert_eq!(
        lineage(&[link("b", "a"), link("b", "x")], "b"),
        vec!["b".to_string()]
    );
    assert_eq!(
        lineage(&[link("a", "b"), link("b", "a")], "a"),
        vec!["a".to_string()]
    );
}

#[test]
fn recovered_from_link_requires_a_distinct_predecessor() {
    let kernel = Kernel::with_v1_reducers();
    for predecessor in ["same", " "] {
        let error = kernel
            .execute(KernelRequest {
                target: "agenty.state.write".to_string(),
                reducer_input: recovered_from("same", predecessor),
                ..KernelRequest::default()
            })
            .unwrap_err();
        assert!(matches!(error, KernelError::InvalidRequest { .. }));
    }
}

#[test]
fn diffy_analyze_emits_task_state_focus_packets() {
    let _lock = git_test_lock().lock().unwrap();
    let dir = tempdir().unwrap();
    setup_diff_repo(dir.path());
    let original_dir = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();

    let kernel =
        Kernel::with_v1_reducers_and_persistence(PersistConfig::new(dir.path().to_path_buf()));
    let response = kernel
        .execute(KernelRequest {
            target: "diffy.analyze".to_string(),
            reducer_input: json!({
                "base": "HEAD~1",
                "head": "HEAD",
                "fail_under_changed": null,
                "fail_under_total": null,
                "fail_under_new": null,
                "max_new_errors": null,
                "max_new_warnings": null,
                "max_new_issues": null,
                "issues": [],
                "issues_state": null,
                "no_issues_state": true,
                "coverage": [fixture("lcov/basic.info")],
                "input": null
            }),
            policy_context: json!({
                "task_id": "task-diff"
            }),
            ..KernelRequest::default()
        })
        .unwrap();

    std::env::set_current_dir(original_dir).unwrap();

    assert_eq!(response.output_packets.len(), 4);
    let focus_packet = response
        .output_packets
        .iter()
        .find(|packet| {
            packet
                .metadata
                .get("event_kind")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind == "focus_set")
        })
        .expect("focus_set packet should be emitted");
    let focus_envelope: suite_packet_core::EnvelopeV1<suite_packet_core::AgentStateEventPayload> =
        serde_json::from_value(focus_packet.body.clone()).unwrap();
    assert_eq!(focus_envelope.payload.paths, vec!["src/alpha.rs"]);

    let snapshot = kernel
        .execute(KernelRequest {
            target: "agenty.state.snapshot".to_string(),
            reducer_input: json!({
                "task_id": "task-diff"
            }),
            policy_context: json!({
                "disable_cache": true
            }),
            ..KernelRequest::default()
        })
        .unwrap();
    let snapshot_envelope: suite_packet_core::EnvelopeV1<suite_packet_core::AgentSnapshotPayload> =
        serde_json::from_value(snapshot.output_packets[0].body.clone()).unwrap();
    assert_eq!(snapshot_envelope.payload.focus_paths, vec!["src/alpha.rs"]);
    assert!(snapshot_envelope
        .payload
        .completed_steps
        .iter()
        .any(|step| step == "diff.analyze"));
}

#[test]
fn contextq_assemble_includes_correlation_findings_for_task() {
    let kernel = Kernel::with_v1_reducers();

    let diff_packet = KernelPacket::from_value(
        serde_json::to_value(
            suite_packet_core::EnvelopeV1 {
                version: "1".to_string(),
                tool: "diffy".to_string(),
                kind: "diff_analyze".to_string(),
                hash: String::new(),
                summary: "changed StopWatch".to_string(),
                files: vec![suite_packet_core::FileRef {
                    path: "src/StopWatch.java".to_string(),
                    relevance: Some(1.0),
                    source: Some("diffy.analyze".to_string()),
                }],
                symbols: Vec::new(),
                risk: None,
                confidence: Some(1.0),
                budget_cost: suite_packet_core::BudgetCost::default(),
                provenance: suite_packet_core::Provenance {
                    inputs: vec!["diff".to_string()],
                    git_base: Some("HEAD~1".to_string()),
                    git_head: Some("HEAD".to_string()),
                    generated_at_unix: 1,
                },
                payload: DiffAnalyzeKernelOutput {
                    gate_result: suite_packet_core::QualityGateResult {
                        passed: true,
                        total_coverage_pct: None,
                        changed_coverage_pct: None,
                        new_file_coverage_pct: None,
                        violations: Vec::new(),
                        issue_counts: None,
                    },
                    diagnostics: None,
                    diffs: vec![SerializableFileDiff {
                        path: "src/StopWatch.java".to_string(),
                        old_path: None,
                        status: suite_packet_core::DiffStatus::Modified,
                        changed_lines: vec![10, 11],
                    }],
                },
            }
            .with_canonical_hash_and_real_budget(),
        )
        .unwrap(),
        Some("diff".to_string()),
    );

    let stack_packet = KernelPacket::from_value(
        serde_json::to_value(stacky_core::slice_to_envelope(
            stacky_core::StackSliceRequest {
                log_text: r#"
java.lang.IllegalStateException: boom
  at org.example.ArrayUtils.run(src/ArrayUtils.java:42)
"#
                .to_string(),
                source: Some("stack.log".to_string()),
                max_failures: None,
            },
        ))
        .unwrap(),
        Some("stack".to_string()),
    );

    let map_packet = KernelPacket::from_value(
        serde_json::to_value(
            suite_packet_core::EnvelopeV1 {
                version: "1".to_string(),
                tool: "mapy".to_string(),
                kind: "repo_map".to_string(),
                hash: String::new(),
                summary: "repo map".to_string(),
                files: vec![
                    suite_packet_core::FileRef {
                        path: "src/StopWatch.java".to_string(),
                        relevance: Some(1.0),
                        source: Some("mapy.repo".to_string()),
                    },
                    suite_packet_core::FileRef {
                        path: "src/ArrayUtils.java".to_string(),
                        relevance: Some(0.8),
                        source: Some("mapy.repo".to_string()),
                    },
                ],
                symbols: Vec::new(),
                risk: None,
                confidence: Some(1.0),
                budget_cost: suite_packet_core::BudgetCost::default(),
                provenance: suite_packet_core::Provenance {
                    inputs: vec!["repo".to_string()],
                    git_base: None,
                    git_head: None,
                    generated_at_unix: 1,
                },
                payload: mapy_core::RepoMapPayload {
                    files_ranked: vec![
                        mapy_core::RankedFile {
                            file_idx: 0,
                            score: 1.0,
                            symbol_count: 1,
                            import_count: 0,
                        },
                        mapy_core::RankedFile {
                            file_idx: 1,
                            score: 0.8,
                            symbol_count: 1,
                            import_count: 0,
                        },
                    ],
                    symbols_ranked: Vec::new(),
                    edges: Vec::new(),
                    focus_hits: Vec::new(),
                    truncation: mapy_core::TruncationSummary::default(),
                },
            }
            .with_canonical_hash_and_real_budget(),
        )
        .unwrap(),
        Some("map".to_string()),
    );

    let response = kernel
        .execute(KernelRequest {
            target: "contextq.assemble".to_string(),
            input_packets: vec![diff_packet, stack_packet, map_packet],
            budget: ExecutionBudget {
                token_cap: Some(1500),
                byte_cap: Some(100_000),
                runtime_ms_cap: None,
            },
            policy_context: json!({
                "task_id": "task-correlation",
                "disable_cache": true
            }),
            ..KernelRequest::default()
        })
        .unwrap();

    let envelope: suite_packet_core::EnvelopeV1<ContextAssembleEnvelopePayload> =
        serde_json::from_value(response.output_packets[0].body.clone()).unwrap();
    let bodies = envelope
        .payload
        .sections
        .iter()
        .map(|section| section.body.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(bodies.contains("appear unrelated to diff"));
}

#[test]
fn contextq_correlate_emits_shared_file_findings_without_diff() {
    let kernel = Kernel::with_v1_reducers();

    let stack_packet = KernelPacket::from_value(
        serde_json::to_value(stacky_core::slice_to_envelope(
            stacky_core::StackSliceRequest {
                log_text: r#"
java.lang.IllegalStateException: boom
  at org.example.StringUtils.run(src/StringUtils.java:42)
"#
                .to_string(),
                source: Some("stack.log".to_string()),
                max_failures: None,
            },
        ))
        .unwrap(),
        Some("stack".to_string()),
    );

    let map_packet = KernelPacket::from_value(
        serde_json::to_value(
            suite_packet_core::EnvelopeV1 {
                version: "1".to_string(),
                tool: "mapy".to_string(),
                kind: "repo_map".to_string(),
                hash: String::new(),
                summary: "repo map".to_string(),
                files: vec![suite_packet_core::FileRef {
                    path: "src/StringUtils.java".to_string(),
                    relevance: Some(1.0),
                    source: Some("mapy.repo".to_string()),
                }],
                symbols: Vec::new(),
                risk: None,
                confidence: Some(1.0),
                budget_cost: suite_packet_core::BudgetCost::default(),
                provenance: suite_packet_core::Provenance {
                    inputs: vec!["repo".to_string()],
                    git_base: None,
                    git_head: None,
                    generated_at_unix: 1,
                },
                payload: mapy_core::RepoMapPayload::default(),
            }
            .with_canonical_hash_and_real_budget(),
        )
        .unwrap(),
        Some("map".to_string()),
    );

    let response = kernel
        .execute(KernelRequest {
            target: "contextq.correlate".to_string(),
            input_packets: vec![stack_packet, map_packet],
            policy_context: json!({"task_id":"task-correlation","scope":"task_first"}),
            ..KernelRequest::default()
        })
        .unwrap();
    let envelope: suite_packet_core::EnvelopeV1<suite_packet_core::ContextCorrelationPayload> =
        serde_json::from_value(response.output_packets[0].body.clone()).unwrap();

    assert!(envelope
        .payload
        .findings
        .iter()
        .any(|finding| finding.rule == "shared_file"));
}

#[test]
fn contextq_correlate_uses_unique_basename_fallback() {
    let kernel = Kernel::with_v1_reducers();

    let stack_packet = KernelPacket::from_value(
        serde_json::to_value(stacky_core::slice_to_envelope(
            stacky_core::StackSliceRequest {
                log_text: r#"
java.lang.IllegalStateException: boom
  at org.example.StringUtils.run(StringUtils.java:42)
"#
                .to_string(),
                source: Some("stack.log".to_string()),
                max_failures: None,
            },
        ))
        .unwrap(),
        Some("stack".to_string()),
    );

    let map_packet = KernelPacket::from_value(
        serde_json::to_value(
            suite_packet_core::EnvelopeV1 {
                version: "1".to_string(),
                tool: "mapy".to_string(),
                kind: "repo_map".to_string(),
                hash: String::new(),
                summary: "repo map".to_string(),
                files: vec![suite_packet_core::FileRef {
                    path: "src/auth/StringUtils.java".to_string(),
                    relevance: Some(1.0),
                    source: Some("mapy.repo".to_string()),
                }],
                symbols: Vec::new(),
                risk: None,
                confidence: Some(1.0),
                budget_cost: suite_packet_core::BudgetCost::default(),
                provenance: suite_packet_core::Provenance {
                    inputs: vec!["repo".to_string()],
                    git_base: None,
                    git_head: None,
                    generated_at_unix: 1,
                },
                payload: mapy_core::RepoMapPayload::default(),
            }
            .with_canonical_hash_and_real_budget(),
        )
        .unwrap(),
        Some("map".to_string()),
    );

    let response = kernel
        .execute(KernelRequest {
            target: "contextq.correlate".to_string(),
            input_packets: vec![stack_packet, map_packet],
            ..KernelRequest::default()
        })
        .unwrap();
    let envelope: suite_packet_core::EnvelopeV1<suite_packet_core::ContextCorrelationPayload> =
        serde_json::from_value(response.output_packets[0].body.clone()).unwrap();
    let finding = envelope
        .payload
        .findings
        .iter()
        .find(|finding| finding.rule == "shared_file")
        .expect("shared_file finding");
    assert!(finding.confidence < 0.74);
    assert!(finding
        .evidence_refs
        .iter()
        .any(|evidence| evidence.kind == "file_basename"));
}

#[test]
fn contextq_manage_reports_checkpoint_deltas_and_working_set() {
    let dir = tempdir().unwrap();
    let mut kernel =
        Kernel::with_v1_reducers_and_persistence(PersistConfig::new(dir.path().to_path_buf()));
    kernel.register_reducer("test.packet", |ctx, _packets| {
        let envelope = suite_packet_core::EnvelopeV1 {
            version: "1".to_string(),
            tool: "contextq".to_string(),
            kind: "context_manage".to_string(),
            hash: String::new(),
            summary: "auth investigation".to_string(),
            files: vec![suite_packet_core::FileRef {
                path: "src/auth.rs".to_string(),
                relevance: Some(1.0),
                source: Some("test.packet".to_string()),
            }],
            symbols: vec![suite_packet_core::SymbolRef {
                name: "authenticate".to_string(),
                file: None,
                kind: Some("function".to_string()),
                relevance: Some(1.0),
                source: Some("test.packet".to_string()),
            }],
            risk: None,
            confidence: Some(1.0),
            budget_cost: suite_packet_core::BudgetCost {
                est_tokens: 48,
                est_bytes: 256,
                runtime_ms: 3,
                tool_calls: 1,
                payload_est_tokens: Some(24),
                payload_est_bytes: Some(128),
            },
            provenance: suite_packet_core::Provenance {
                inputs: vec!["task:task-manage".to_string()],
                git_base: None,
                git_head: None,
                generated_at_unix: 1,
            },
            payload: json!({"task_id":"task-manage","summary":"auth investigation"}),
        }
        .with_canonical_hash_and_real_budget();
        Ok(ReducerResult {
            output_packets: vec![KernelPacket::from_value(
                serde_json::to_value(envelope).unwrap(),
                Some(format!("packet-{}", ctx.request_id)),
            )],
            metadata: json!({"task_id":"task-manage"}),
        })
    });

    kernel
        .execute(KernelRequest {
            target: "test.packet".to_string(),
            ..KernelRequest::default()
        })
        .unwrap();
    for event in [
        json!({
            "task_id": "task-manage",
            "event_id": "checkpoint-1",
            "occurred_at_unix": 1,
            "actor": "agent",
            "kind": "checkpoint_saved",
            "paths": [],
            "symbols": [],
            "data": {"type":"checkpoint_saved","checkpoint_id":"ckpt-1"}
        }),
        json!({
            "task_id": "task-manage",
            "event_id": "edit-1",
            "occurred_at_unix": 2,
            "actor": "agent",
            "kind": "file_edited",
            "paths": ["src/auth.rs"],
            "symbols": ["authenticate"],
            "data": {"type":"file_edited","regions":[]}
        }),
    ] {
        kernel
            .execute(KernelRequest {
                target: "agenty.state.write".to_string(),
                reducer_input: event,
                ..KernelRequest::default()
            })
            .unwrap();
    }

    let response = kernel
        .execute(KernelRequest {
            target: "contextq.manage".to_string(),
            reducer_input: json!({
                "task_id": "task-manage",
                "budget_tokens": 256,
                "budget_bytes": 4096,
                "scope": "task_first"
            }),
            ..KernelRequest::default()
        })
        .unwrap();
    let envelope: suite_packet_core::EnvelopeV1<suite_packet_core::ContextManagePayload> =
        serde_json::from_value(response.output_packets[0].body.clone()).unwrap();

    assert_eq!(envelope.payload.task_id, "task-manage");
    assert!(envelope
        .payload
        .changed_paths_since_checkpoint
        .contains(&"src/auth.rs".to_string()));
    assert!(!envelope.payload.working_set.is_empty());
}

#[test]
fn contextq_manage_uses_focus_filters_to_prefer_matching_packets() {
    let dir = tempdir().unwrap();
    let mut kernel =
        Kernel::with_v1_reducers_and_persistence(PersistConfig::new(dir.path().to_path_buf()));
    kernel.register_reducer("test.auth_packet", |ctx, _packets| {
        let envelope = suite_packet_core::EnvelopeV1 {
            version: "1".to_string(),
            tool: "contextq".to_string(),
            kind: "context_manage".to_string(),
            hash: String::new(),
            summary: "investigation notes".to_string(),
            files: vec![suite_packet_core::FileRef {
                path: "src/auth.rs".to_string(),
                relevance: Some(1.0),
                source: Some("test.auth_packet".to_string()),
            }],
            symbols: vec![suite_packet_core::SymbolRef {
                name: "authenticate".to_string(),
                file: Some("src/auth.rs".to_string()),
                kind: Some("function".to_string()),
                relevance: Some(1.0),
                source: Some("test.auth_packet".to_string()),
            }],
            risk: None,
            confidence: Some(1.0),
            budget_cost: suite_packet_core::BudgetCost {
                est_tokens: 48,
                est_bytes: 256,
                runtime_ms: 3,
                tool_calls: 1,
                payload_est_tokens: Some(24),
                payload_est_bytes: Some(128),
            },
            provenance: suite_packet_core::Provenance {
                inputs: vec!["task:task-manage".to_string()],
                git_base: None,
                git_head: None,
                generated_at_unix: 1,
            },
            payload: json!({"task_id":"task-manage","summary":"investigation notes"}),
        }
        .with_canonical_hash_and_real_budget();
        Ok(ReducerResult {
            output_packets: vec![KernelPacket::from_value(
                serde_json::to_value(envelope).unwrap(),
                Some(format!("packet-auth-{}", ctx.request_id)),
            )],
            metadata: json!({"task_id":"task-manage"}),
        })
    });
    kernel.register_reducer("test.other_packet", |ctx, _packets| {
        let envelope = suite_packet_core::EnvelopeV1 {
            version: "1".to_string(),
            tool: "contextq".to_string(),
            kind: "context_manage".to_string(),
            hash: String::new(),
            summary: "investigation notes".to_string(),
            files: vec![suite_packet_core::FileRef {
                path: "src/billing.rs".to_string(),
                relevance: Some(1.0),
                source: Some("test.other_packet".to_string()),
            }],
            symbols: vec![suite_packet_core::SymbolRef {
                name: "invoice".to_string(),
                file: Some("src/billing.rs".to_string()),
                kind: Some("function".to_string()),
                relevance: Some(1.0),
                source: Some("test.other_packet".to_string()),
            }],
            risk: None,
            confidence: Some(1.0),
            budget_cost: suite_packet_core::BudgetCost {
                est_tokens: 48,
                est_bytes: 256,
                runtime_ms: 3,
                tool_calls: 1,
                payload_est_tokens: Some(24),
                payload_est_bytes: Some(128),
            },
            provenance: suite_packet_core::Provenance {
                inputs: vec!["task:task-manage".to_string()],
                git_base: None,
                git_head: None,
                generated_at_unix: 1,
            },
            payload: json!({"task_id":"task-manage","summary":"investigation notes"}),
        }
        .with_canonical_hash_and_real_budget();
        Ok(ReducerResult {
            output_packets: vec![KernelPacket::from_value(
                serde_json::to_value(envelope).unwrap(),
                Some(format!("packet-other-{}", ctx.request_id)),
            )],
            metadata: json!({"task_id":"task-manage"}),
        })
    });

    kernel
        .execute(KernelRequest {
            target: "test.auth_packet".to_string(),
            ..KernelRequest::default()
        })
        .unwrap();
    kernel
        .execute(KernelRequest {
            target: "test.other_packet".to_string(),
            ..KernelRequest::default()
        })
        .unwrap();

    let response = kernel
        .execute(KernelRequest {
            target: "contextq.manage".to_string(),
            reducer_input: json!({
                "task_id": "task-manage",
                "query": "investigation notes",
                "budget_tokens": 256,
                "budget_bytes": 4096,
                "scope": "task_first",
                "focus_paths": ["src/auth.rs"],
                "focus_symbols": ["authenticate"]
            }),
            ..KernelRequest::default()
        })
        .unwrap();
    let envelope: suite_packet_core::EnvelopeV1<suite_packet_core::ContextManagePayload> =
        serde_json::from_value(response.output_packets[0].body.clone()).unwrap();

    assert_eq!(envelope.payload.working_set.len(), 1);
    assert_eq!(envelope.payload.working_set[0].target, "test.auth_packet");
}

#[test]
fn contextq_assemble_uses_task_snapshot_to_compress_read_sections() {
    let dir = tempdir().unwrap();
    let kernel =
        Kernel::with_v1_reducers_and_persistence(PersistConfig::new(dir.path().to_path_buf()));
    kernel
        .execute(KernelRequest {
            target: "agenty.state.write".to_string(),
            reducer_input: json!({
                "task_id": "task-a",
                "event_id": "evt-1",
                "occurred_at_unix": 1,
                "actor": "agent",
                "kind": "file_read",
                "paths": ["src/time/StopWatch.java"],
                "data": {"type": "file_read"}
            }),
            ..KernelRequest::default()
        })
        .unwrap();

    let packet = KernelPacket::from_value(
        json!({
            "packet_id": "diffy",
            "sections": [{
                "title": "Diff",
                "body": "StopWatch.java changed on lines 10-20",
                "refs": [{"kind": "file", "value": "src/time/StopWatch.java"}],
                "relevance": 0.9
            }]
        }),
        None,
    );
    let response = kernel
        .execute(KernelRequest {
            target: "contextq.assemble".to_string(),
            input_packets: vec![packet],
            policy_context: json!({
                "task_id": "task-a",
                "disable_cache": true
            }),
            ..KernelRequest::default()
        })
        .unwrap();
    let envelope: suite_packet_core::EnvelopeV1<ContextAssembleEnvelopePayload> =
        serde_json::from_value(response.output_packets[0].body.clone()).unwrap();
    assert!(envelope.payload.sections[0]
        .body
        .starts_with("Reminder: already reviewed"));
}

#[test]
fn contextq_assemble_can_augment_with_task_memory() {
    let dir = tempdir().unwrap();
    let persistence = PersistConfig::new(dir.path().to_path_buf());
    let kernel = Kernel::with_v1_reducers_and_persistence(persistence.clone());
    let cache_owner = CachePersistence::open(persistence).unwrap();

    kernel
        .execute(KernelRequest {
            target: "agenty.state.write".to_string(),
            reducer_input: json!({
                "task_id": "task-memory",
                "event_id": "evt-edit",
                "occurred_at_unix": 1,
                "actor": "agent",
                "kind": "file_edited",
                "paths": ["src/auth/Login.java"],
                "symbols": ["authenticate"],
                "data": {"type": "file_edited", "regions": []}
            }),
            ..KernelRequest::default()
        })
        .unwrap();

    {
        let shared_cache = cache_owner.shared_cache();
        let mut cache = shared_cache.lock().unwrap();
        let mut hooks = NoopDeltaReuseHooks;
        let lookup = cache.lookup_with_hooks(
            "diffy.analyze",
            &json!({"task_id":"task-memory"}),
            &mut hooks,
        );
        cache.put_with_hooks(
            "diffy.analyze",
            &lookup,
            vec![CachePacket {
                body: serde_json::to_value(
                    suite_packet_core::EnvelopeV1 {
                        version: "1".to_string(),
                        tool: "diffy".to_string(),
                        kind: "diff_analyze".to_string(),
                        hash: String::new(),
                        summary: "authentication fix in src/auth/Login.java".to_string(),
                        files: vec![suite_packet_core::FileRef {
                            path: "src/auth/Login.java".to_string(),
                            relevance: Some(1.0),
                            source: Some("diffy.analyze".to_string()),
                        }],
                        symbols: vec![suite_packet_core::SymbolRef {
                            name: "authenticate".to_string(),
                            file: Some("src/auth/Login.java".to_string()),
                            kind: Some("method".to_string()),
                            relevance: Some(1.0),
                            source: Some("diffy.analyze".to_string()),
                        }],
                        risk: None,
                        confidence: Some(1.0),
                        budget_cost: suite_packet_core::BudgetCost {
                            est_tokens: 80,
                            est_bytes: 512,
                            runtime_ms: 10,
                            tool_calls: 1,
                            payload_est_tokens: None,
                            payload_est_bytes: None,
                        },
                        provenance: suite_packet_core::Provenance {
                            inputs: vec!["task:task-memory".to_string()],
                            git_base: None,
                            git_head: None,
                            generated_at_unix: 2,
                        },
                        payload: DiffAnalyzeKernelOutput {
                            gate_result: suite_packet_core::QualityGateResult {
                                passed: true,
                                total_coverage_pct: None,
                                changed_coverage_pct: None,
                                new_file_coverage_pct: None,
                                violations: Vec::new(),
                                issue_counts: None,
                            },
                            diagnostics: None,
                            diffs: vec![SerializableFileDiff {
                                path: "src/auth/Login.java".to_string(),
                                old_path: None,
                                status: suite_packet_core::DiffStatus::Modified,
                                changed_lines: vec![10, 11],
                            }],
                        },
                    }
                    .with_canonical_hash_and_real_budget(),
                )
                .unwrap(),
                metadata: json!({"task_id":"task-memory"}),
                token_usage: Some(80),
                runtime_ms: Some(10),
                ..CachePacket::default()
            }],
            json!({"task_id":"task-memory"}),
            &mut hooks,
        );
    }

    let seed_packet = KernelPacket::from_value(
        json!({
            "packet_id": "seed",
            "tool": "stacky",
            "kind": "stack_slice",
            "summary": "seed packet",
            "budget_cost": {"est_tokens": 20, "est_bytes": 128, "runtime_ms": 1},
            "payload": {"total_failures": 1, "unique_failures": 1}
        }),
        None,
    );

    let response = kernel
        .execute(KernelRequest {
            target: "contextq.assemble".to_string(),
            input_packets: vec![seed_packet],
            policy_context: json!({
                "task_id": "task-memory",
                "include_task_memory": true,
            }),
            budget: ExecutionBudget {
                token_cap: Some(500),
                byte_cap: Some(20_000),
                runtime_ms_cap: None,
            },
            ..KernelRequest::default()
        })
        .unwrap();

    let envelope: suite_packet_core::EnvelopeV1<ContextAssembleEnvelopePayload> =
        serde_json::from_value(response.output_packets[0].body.clone()).unwrap();
    assert!(envelope
        .payload
        .refs
        .iter()
        .any(|reference| reference.value == "src/auth/Login.java"));
    cache_owner
        .shutdown(std::time::Duration::from_secs(2))
        .unwrap();
}

#[test]
fn execute_sequence_with_observer_emits_live_step_events_in_order() {
    #[derive(Default)]
    struct RecordingObserver {
        events: Vec<String>,
    }

    impl SequenceObserver for RecordingObserver {
        fn on_step_started(&mut self, _position: usize, step: &KernelStepRequest) {
            self.events.push(format!("started:{}", step.id));
        }

        fn on_step_completed(
            &mut self,
            _position: usize,
            step: &KernelStepRequest,
            _response: &KernelResponse,
        ) {
            self.events.push(format!("completed:{}", step.id));
        }

        fn on_step_failed(
            &mut self,
            _position: usize,
            step: &KernelStepRequest,
            _failure: &KernelFailure,
        ) {
            self.events.push(format!("failed:{}", step.id));
        }
    }

    let mut kernel = Kernel::new();
    kernel.register_reducer("demo.ok", |_ctx, _input| {
        Ok(ReducerResult {
            output_packets: Vec::new(),
            metadata: Value::Null,
        })
    });
    kernel.register_reducer("demo.fail", |_ctx, _input| {
        Err(KernelError::ReducerFailed {
            target: "demo.fail".to_string(),
            detail: "boom".to_string(),
        })
    });

    let mut observer = RecordingObserver::default();
    let response = kernel
        .execute_sequence_with_observer(
            KernelSequenceRequest {
                steps: vec![
                    KernelStepRequest {
                        id: "one".to_string(),
                        target: "demo.ok".to_string(),
                        ..KernelStepRequest::default()
                    },
                    KernelStepRequest {
                        id: "two".to_string(),
                        target: "demo.fail".to_string(),
                        depends_on: vec!["one".to_string()],
                        ..KernelStepRequest::default()
                    },
                ],
                ..KernelSequenceRequest::default()
            },
            &mut observer,
        )
        .unwrap();

    assert_eq!(response.step_results.len(), 2);
    assert_eq!(
        observer.events,
        vec![
            "started:one".to_string(),
            "completed:one".to_string(),
            "started:two".to_string(),
            "failed:two".to_string(),
        ]
    );
}

#[test]
fn loads_packet_file() {
    let dir = tempdir().unwrap();
    let packet_path = dir.path().join("packet.json");
    std::fs::write(&packet_path, r#"{"packet_id":"a","payload":{"k":"v"}}"#).unwrap();

    let packet = load_packet_file(&packet_path).unwrap();
    assert_eq!(packet.packet_id.as_deref(), Some("a"));
}

fn instruction_request(mode: suite_packet_core::InstructionRenderMode) -> KernelRequest {
    KernelRequest {
        target: "packet28.instruction.summarize".to_string(),
        reducer_input: serde_json::to_value(InstructionSummaryRequest {
            path: "AGENTS.md".to_string(),
            content: "# Coverage\n\n- Prefer deterministic reducers.\n- Keep tool activity compact.\n\n## Auth\nTouch src/auth.rs carefully and preserve cache keys.\n\n## Cache\nKeep snapshot invalidation correct.\n".to_string(),
            content_sha256: "untrusted-caller-hint".to_string(),
            mode,
            stable_config: suite_packet_core::InstructionStableConfig {
                focus_terms: vec!["auth".to_string()],
                ..suite_packet_core::InstructionStableConfig::default()
            },
            task_id: Some("task-auth".to_string()),
            budget_tokens: Some(128),
            schema_version: 1,
            source_kind: Some("instruction_file".to_string()),
            backend_kind: Some("linux_preload".to_string()),
            agent_family: Some("codex".to_string()),
        })
        .unwrap(),
        policy_context: json!({
            "task_id": "task-auth",
            "instruction_mode": mode,
        }),
        ..KernelRequest::default()
    }
}

fn instruction_payload(response: &KernelResponse) -> InstructionSummaryPayload {
    let packet = response.output_packets.first().unwrap();
    let envelope: suite_packet_core::EnvelopeV1<InstructionSummaryPayload> =
        serde_json::from_value(packet.body.clone()).unwrap();
    envelope.payload
}

#[test]
fn instruction_summary_stable_mode_reuses_metadata_independent_cache() {
    let dir = tempdir().unwrap();
    let kernel =
        Kernel::with_v1_reducers_and_persistence(PersistConfig::new(dir.path().to_path_buf()));
    let request = instruction_request(suite_packet_core::InstructionRenderMode::Stable);

    let first = kernel.execute(request.clone()).unwrap();
    let first_payload = instruction_payload(&first);
    assert!(first_payload.summary_text.starts_with("# [p28:stable:v1]"));
    assert!(!first_payload.summary_text.contains("task-auth"));
    assert_eq!(first_payload.task_label, "stable");
    assert_eq!(first_payload.backend_kind, "backend_independent");
    assert_eq!(first_payload.agent_family, "agent_independent");
    assert_ne!(first_payload.content_sha256, "untrusted-caller-hint");
    assert_eq!(first.metadata["cache"]["hit"].as_bool(), Some(false));
    assert_eq!(
        first.metadata["cache"]["miss_reason"].as_str(),
        Some("not_found")
    );

    let mut changed_metadata = request;
    changed_metadata.reducer_input["task_id"] = json!("task-other");
    changed_metadata.reducer_input["backend_kind"] = json!("macos_swap");
    changed_metadata.reducer_input["agent_family"] = json!("claude");
    changed_metadata.policy_context = json!({
        "task_id": "task-other",
        "instruction_mode": "stable",
    });
    let second = kernel.execute(changed_metadata).unwrap();
    assert_eq!(second.metadata["cache"]["hit"].as_bool(), Some(true));
    assert_eq!(
        instruction_payload(&second).rendered_sha256,
        first_payload.rendered_sha256
    );
}

#[test]
fn instruction_summary_stable_cache_key_covers_every_stable_input() {
    let dir = tempdir().unwrap();
    let kernel =
        Kernel::with_v1_reducers_and_persistence(PersistConfig::new(dir.path().to_path_buf()));
    let base_request = instruction_request(suite_packet_core::InstructionRenderMode::Stable);

    let first = kernel.execute(base_request.clone()).unwrap();
    assert_eq!(first.metadata["cache"]["hit"].as_bool(), Some(false));

    let mut caller_hash_only = base_request.clone();
    caller_hash_only.reducer_input["content_sha256"] = json!("another-untrusted-hint");
    assert_eq!(
        kernel.execute(caller_hash_only).unwrap().metadata["cache"]["hit"].as_bool(),
        Some(true),
        "caller-provided source hashes must not alter cache identity"
    );
    let mut normalized_config = base_request.clone();
    normalized_config.reducer_input["stable_config"]["focus_terms"] = json!([" AUTH ", "auth"]);
    assert_eq!(
        kernel.execute(normalized_config).unwrap().metadata["cache"]["hit"].as_bool(),
        Some(true),
        "semantically equivalent stable config must share cache identity"
    );

    let mut variants = Vec::new();
    let mut source = base_request.clone();
    source.reducer_input["content"] = json!("# Changed\n\nDifferent source bytes.\n");
    variants.push(source);
    let mut path = base_request.clone();
    path.reducer_input["path"] = json!("CLAUDE.md");
    variants.push(path);
    let mut schema = base_request.clone();
    schema.reducer_input["schema_version"] = json!(2);
    variants.push(schema);
    let mut budget = base_request.clone();
    budget.reducer_input["budget_tokens"] = json!(256);
    variants.push(budget);
    let mut config = base_request;
    config.reducer_input["stable_config"]["focus_terms"] = json!(["cache"]);
    variants.push(config);

    for variant in variants {
        let response = kernel.execute(variant).unwrap();
        assert_eq!(response.metadata["cache"]["hit"].as_bool(), Some(false));
    }
}

#[test]
fn instruction_summary_defaults_to_exact_passthrough_without_cache() {
    let dir = tempdir().unwrap();
    let kernel =
        Kernel::with_v1_reducers_and_persistence(PersistConfig::new(dir.path().to_path_buf()));
    let source = "# Exact\r\n\r\n  Preserve spacing.\r\n";
    let request = KernelRequest {
        target: "packet28.instruction.summarize".to_string(),
        reducer_input: json!({
            "path": "AGENTS.md",
            "content": source,
            "content_sha256": "not-trusted",
            "task_id": "task-a",
            "schema_version": 1,
        }),
        ..KernelRequest::default()
    };

    let first = kernel.execute(request.clone()).unwrap();
    let second = kernel.execute(request).unwrap();

    assert_eq!(instruction_payload(&first).summary_text, source);
    assert_eq!(instruction_payload(&second).summary_text, source);
    assert_eq!(first.metadata["cache"]["hit"].as_bool(), Some(false));
    assert_eq!(second.metadata["cache"]["hit"].as_bool(), Some(false));
    assert_eq!(first.metadata["cache"]["key"].as_str(), Some(""));
    assert_eq!(second.metadata["cache"]["key"].as_str(), Some(""));
    assert_eq!(
        first.metadata["cache"]["miss_reason"].as_str(),
        Some("disabled")
    );
    assert_eq!(
        second.metadata["cache"]["miss_reason"].as_str(),
        Some("disabled")
    );
}

#[test]
fn stable_render_is_byte_identical_across_task_and_snapshot_changes() {
    let request = InstructionSummaryRequest {
        mode: suite_packet_core::InstructionRenderMode::Stable,
        ..serde_json::from_value(
            instruction_request(suite_packet_core::InstructionRenderMode::Stable).reducer_input,
        )
        .unwrap()
    };
    let first_snapshot = suite_packet_core::AgentSnapshotPayload {
        task_id: "task-a".to_string(),
        focus_paths: vec!["src/auth.rs".to_string()],
        ..suite_packet_core::AgentSnapshotPayload::default()
    };
    let second_snapshot = suite_packet_core::AgentSnapshotPayload {
        task_id: "task-b".to_string(),
        focus_paths: vec!["src/cache.rs".to_string()],
        event_count: 99,
        ..suite_packet_core::AgentSnapshotPayload::default()
    };

    let first = render_instruction(&request, Some(&first_snapshot)).unwrap();
    let mut other_task = request;
    other_task.task_id = Some("task-b".to_string());
    other_task.backend_kind = Some("macos_swap".to_string());
    other_task.agent_family = Some("claude".to_string());
    let second = render_instruction(&other_task, Some(&second_snapshot)).unwrap();

    assert_eq!(first.summary_text(), second.summary_text());
    assert_eq!(first.rendered_sha256(), second.rendered_sha256());
    assert_eq!(first.snapshot_sha256(), None);
    assert_eq!(second.snapshot_sha256(), None);
}

#[test]
fn adaptive_render_fingerprints_untrusted_task_identity_within_budget() {
    let mut request = InstructionSummaryRequest {
        mode: suite_packet_core::InstructionRenderMode::Adaptive,
        ..serde_json::from_value(
            instruction_request(suite_packet_core::InstructionRenderMode::Adaptive).reducer_input,
        )
        .unwrap()
    };
    request.task_id = Some(format!(
        "task-auth\r\n# injected-heading {}",
        "oversized".repeat(2_048)
    ));
    request.budget_tokens = Some(96);

    let rendered = render_instruction(&request, None).unwrap();
    let header = rendered.summary_text().lines().next().unwrap_or_default();
    let task_identity = header
        .split(" task:")
        .nth(1)
        .and_then(|suffix| suffix.split_whitespace().next())
        .unwrap_or_default();

    assert_eq!(rendered.summary_text().len(), 384);
    assert!(!rendered.summary_text().contains('\r'));
    assert!(!rendered.summary_text().contains("# injected-heading"));
    assert!(!header.contains("task-auth"));
    assert!(task_identity.starts_with("sha256-"));
    assert_eq!(
        task_identity.trim_start_matches("sha256-").len(),
        instruction_runtime::ADAPTIVE_TASK_FINGERPRINT_CHARS
    );
    assert!(task_identity
        .trim_start_matches("sha256-")
        .chars()
        .all(|ch| ch.is_ascii_hexdigit()));
}

#[test]
fn adaptive_render_normalizes_task_identity_before_fingerprinting() {
    let request = InstructionSummaryRequest {
        mode: suite_packet_core::InstructionRenderMode::Adaptive,
        task_id: Some(" task-auth \r\n".to_string()),
        ..serde_json::from_value(
            instruction_request(suite_packet_core::InstructionRenderMode::Adaptive).reducer_input,
        )
        .unwrap()
    };
    let mut normalized = request.clone();
    normalized.task_id = Some("task-auth".to_string());

    let first = render_instruction(&request, None).unwrap();
    let second = render_instruction(&normalized, None).unwrap();

    assert_eq!(first.summary_text(), second.summary_text());
    assert_eq!(first.rendered_sha256(), second.rendered_sha256());
}

#[test]
fn adaptive_render_uses_snapshot_set_semantics_for_identity_and_bytes() {
    let request = InstructionSummaryRequest {
        mode: suite_packet_core::InstructionRenderMode::Adaptive,
        ..serde_json::from_value(
            instruction_request(suite_packet_core::InstructionRenderMode::Adaptive).reducer_input,
        )
        .unwrap()
    };
    let first_snapshot = suite_packet_core::AgentSnapshotPayload {
        focus_paths: vec!["src/auth.rs".to_string(), "src/cache.rs".to_string()],
        focus_symbols: vec!["Authenticate".to_string(), "CacheKey".to_string()],
        open_questions: vec![
            suite_packet_core::AgentQuestion {
                id: "q-1".to_string(),
                text: "Which cache key?".to_string(),
            },
            suite_packet_core::AgentQuestion {
                id: "q-2".to_string(),
                text: "Should auth retry?".to_string(),
            },
        ],
        ..suite_packet_core::AgentSnapshotPayload::default()
    };
    let reordered_with_duplicates = suite_packet_core::AgentSnapshotPayload {
        focus_paths: vec![
            "src/cache.rs".to_string(),
            "src/auth.rs".to_string(),
            "src/auth.rs".to_string(),
        ],
        focus_symbols: vec![
            "CacheKey".to_string(),
            "Authenticate".to_string(),
            "Authenticate".to_string(),
        ],
        open_questions: vec![
            suite_packet_core::AgentQuestion {
                id: "q-2-other".to_string(),
                text: "Should auth retry?".to_string(),
            },
            suite_packet_core::AgentQuestion {
                id: "q-1-other".to_string(),
                text: "Which cache key?".to_string(),
            },
            suite_packet_core::AgentQuestion {
                id: "duplicate".to_string(),
                text: "Which cache key?".to_string(),
            },
        ],
        ..suite_packet_core::AgentSnapshotPayload::default()
    };

    let first = render_instruction(&request, Some(&first_snapshot)).unwrap();
    let reordered = render_instruction(&request, Some(&reordered_with_duplicates)).unwrap();
    assert_eq!(first.snapshot_sha256(), reordered.snapshot_sha256());
    assert_eq!(first.summary_text(), reordered.summary_text());
    assert_eq!(first.rendered_sha256(), reordered.rendered_sha256());

    let changed_membership = suite_packet_core::AgentSnapshotPayload {
        focus_paths: vec!["src/broker.rs".to_string()],
        ..first_snapshot
    };
    let changed = render_instruction(&request, Some(&changed_membership)).unwrap();
    assert_ne!(first.snapshot_sha256(), changed.snapshot_sha256());
    assert_ne!(first.summary_text(), changed.summary_text());
}

#[test]
fn adaptive_snapshot_drift_changes_bytes_and_never_reuses_cache() {
    let dir = tempdir().unwrap();
    let kernel =
        Kernel::with_v1_reducers_and_persistence(PersistConfig::new(dir.path().to_path_buf()));
    let request = instruction_request(suite_packet_core::InstructionRenderMode::Adaptive);

    kernel
        .execute(KernelRequest {
            target: "agenty.state.write".to_string(),
            reducer_input: json!({
                "task_id": "task-auth",
                "event_id": "focus-auth",
                "occurred_at_unix": 1,
                "actor": "agent",
                "kind": "focus_set",
                "paths": ["src/auth.rs"],
                "symbols": ["authenticate"],
                "data": {"type": "focus_set"}
            }),
            ..KernelRequest::default()
        })
        .unwrap();
    let first = kernel.execute(request.clone()).unwrap();

    kernel
        .execute(KernelRequest {
            target: "agenty.state.write".to_string(),
            reducer_input: json!({
                "task_id": "task-auth",
                "event_id": "focus-cache",
                "occurred_at_unix": 2,
                "actor": "agent",
                "kind": "focus_set",
                "paths": ["src/cache.rs"],
                "symbols": ["invalidate_cache"],
                "data": {"type": "focus_set"}
            }),
            ..KernelRequest::default()
        })
        .unwrap();
    let second = kernel.execute(request).unwrap();
    let first_payload = instruction_payload(&first);
    let second_payload = instruction_payload(&second);

    assert_eq!(first.metadata["cache"]["hit"].as_bool(), Some(false));
    assert_eq!(second.metadata["cache"]["hit"].as_bool(), Some(false));
    assert_eq!(first.metadata["cache"]["key"].as_str(), Some(""));
    assert_eq!(second.metadata["cache"]["key"].as_str(), Some(""));
    assert_eq!(
        first.metadata["cache"]["miss_reason"].as_str(),
        Some("disabled")
    );
    assert_eq!(
        second.metadata["cache"]["miss_reason"].as_str(),
        Some("disabled")
    );
    assert_ne!(
        first_payload.snapshot_sha256,
        second_payload.snapshot_sha256
    );
    assert_ne!(
        first_payload.rendered_sha256,
        second_payload.rendered_sha256
    );
}
