use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use roaring::RoaringBitmap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use thiserror::Error;

use context_memory_core::{
    basename_alias, normalize_context_path, CachePacket, DeltaReuseHooks, NoopDeltaReuseHooks,
    PacketCache, RecallHit, RecallOptions, RecallScope,
};

pub use context_memory_core::PersistConfig;

mod kernel_types;
mod kernel_runtime;
mod kernel_registry;
mod diff_runtime;
mod agenty_runtime;
mod contextq_runtime;
mod tool_reducers_runtime;
mod correlation_runtime;
mod governance_runtime;
mod reactive_runtime;

pub use diff_runtime::*;
pub use kernel_types::*;
pub use kernel_runtime::*;
pub use kernel_registry::*;
pub(crate) use agenty_runtime::*;
pub(crate) use correlation_runtime::*;
pub(crate) use contextq_runtime::*;
pub(crate) use governance_runtime::*;
pub(crate) use reactive_runtime::*;
pub(crate) use tool_reducers_runtime::*;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct ContextAssembleEnvelopePayload {
    sources: Vec<String>,
    sections: Vec<contextq_core::ContextSection>,
    refs: Vec<contextq_core::ContextRef>,
    truncated: bool,
    assembly: contextq_core::AssemblySummary,
    tool_invocations: Vec<contextq_core::ToolInvocation>,
    reducer_invocations: Vec<contextq_core::ReducerInvocation>,
    text_blobs: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    debug: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct ContextManageRequest {
    task_id: String,
    query: Option<String>,
    budget_tokens: u64,
    budget_bytes: usize,
    scope: RecallScope,
    checkpoint_id: Option<String>,
    focus_paths: Vec<String>,
    focus_symbols: Vec<String>,
}

impl Default for ContextManageRequest {
    fn default() -> Self {
        Self {
            task_id: String::new(),
            query: None,
            budget_tokens: contextq_core::DEFAULT_BUDGET_TOKENS,
            budget_bytes: contextq_core::DEFAULT_BUDGET_BYTES,
            scope: RecallScope::TaskFirst,
            checkpoint_id: None,
            focus_paths: Vec::new(),
            focus_symbols: Vec::new(),
        }
    }
}

fn path_matches_any(patterns: &[String], candidate: &str) -> bool {
    patterns.iter().any(|pattern| {
        let pattern = pattern.trim();
        !pattern.is_empty()
            && (candidate == pattern
                || candidate.starts_with(pattern)
                || pattern.starts_with(candidate)
                || candidate.contains(pattern))
    })
}

fn estimate_tokens(bytes: usize) -> u64 {
    (bytes as u64).div_ceil(4)
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn merge_json(left: Value, right: Value) -> Value {
    match (left, right) {
        (Value::Object(mut left), Value::Object(right)) => {
            for (key, value) in right {
                left.insert(key, value);
            }
            Value::Object(left)
        }
        (value, Value::Null) => value,
        (_, value) => value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::AtomicU64;
    use std::sync::{Arc, Mutex, OnceLock};
    use tempfile::tempdir;

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
        assert!(status.success(), "git {:?} failed with {status}", args);
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
    fn caches_reducer_packets_by_request_hash() {
        let mut kernel = Kernel::new();
        let calls = Arc::new(AtomicU64::new(0));
        let calls_ref = calls.clone();
        kernel.register_reducer("count.reducer", move |_ctx, _packets| {
            calls_ref.fetch_add(1, Ordering::Relaxed);
            Ok(ReducerResult {
                output_packets: vec![KernelPacket::from_value(json!({"ok": true}), None)],
                metadata: json!({"source":"reducer"}),
            })
        });

        let request = KernelRequest {
            target: "count.reducer".to_string(),
            reducer_input: json!({"task":"same"}),
            ..KernelRequest::default()
        };

        let first = kernel.execute(request.clone()).unwrap();
        let second = kernel.execute(request).unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(first.output_packets.len(), 1);
        assert_eq!(second.output_packets.len(), 1);
        assert_eq!(
            second
                .metadata
                .get("cache")
                .and_then(|v| v.get("hit"))
                .and_then(Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn cache_fingerprint_changes_force_cache_miss() {
        let mut kernel = Kernel::new();
        let calls = Arc::new(AtomicU64::new(0));
        let calls_ref = calls.clone();
        kernel.register_reducer("count.reducer", move |_ctx, _packets| {
            calls_ref.fetch_add(1, Ordering::Relaxed);
            Ok(ReducerResult {
                output_packets: vec![KernelPacket::from_value(json!({"ok": true}), None)],
                metadata: json!({"source":"reducer"}),
            })
        });

        let mut request = KernelRequest {
            target: "count.reducer".to_string(),
            reducer_input: json!({"task":"same"}),
            policy_context: json!({"cache_fingerprint":"fp-1"}),
            ..KernelRequest::default()
        };

        let first = kernel.execute(request.clone()).unwrap();
        assert_eq!(
            first
                .metadata
                .get("cache")
                .and_then(|v| v.get("hit"))
                .and_then(Value::as_bool),
            Some(false)
        );

        let second = kernel.execute(request.clone()).unwrap();
        assert_eq!(
            second
                .metadata
                .get("cache")
                .and_then(|v| v.get("hit"))
                .and_then(Value::as_bool),
            Some(true)
        );

        request.policy_context = json!({"cache_fingerprint":"fp-2"});
        let third = kernel.execute(request).unwrap();
        assert_eq!(
            third
                .metadata
                .get("cache")
                .and_then(|v| v.get("hit"))
                .and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn persistent_kernel_reuses_cache_across_instances() {
        let dir = tempdir().unwrap();
        let config = PersistConfig::new(dir.path().to_path_buf());

        let first_calls = Arc::new(AtomicU64::new(0));
        let first_calls_ref = first_calls.clone();
        let mut first_kernel = Kernel::with_v1_reducers_and_persistence(config.clone());
        first_kernel.register_reducer("count.reducer", move |_ctx, _packets| {
            first_calls_ref.fetch_add(1, Ordering::Relaxed);
            Ok(ReducerResult {
                output_packets: vec![KernelPacket::from_value(json!({"ok": true}), None)],
                metadata: json!({"source":"reducer"}),
            })
        });

        let request = KernelRequest {
            target: "count.reducer".to_string(),
            reducer_input: json!({"task":"persisted"}),
            ..KernelRequest::default()
        };

        let first = first_kernel.execute(request.clone()).unwrap();
        assert_eq!(first_calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            first
                .metadata
                .get("cache")
                .and_then(|v| v.get("hit"))
                .and_then(Value::as_bool),
            Some(false)
        );
        drop(first_kernel);

        let second_calls = Arc::new(AtomicU64::new(0));
        let second_calls_ref = second_calls.clone();
        let mut second_kernel = Kernel::with_v1_reducers_and_persistence(config);
        second_kernel.register_reducer("count.reducer", move |_ctx, _packets| {
            second_calls_ref.fetch_add(1, Ordering::Relaxed);
            Ok(ReducerResult {
                output_packets: vec![KernelPacket::from_value(json!({"ok": true}), None)],
                metadata: json!({"source":"reducer"}),
            })
        });

        let second = second_kernel.execute(request).unwrap();
        assert_eq!(second_calls.load(Ordering::Relaxed), 0);
        assert_eq!(
            second
                .metadata
                .get("cache")
                .and_then(|v| v.get("hit"))
                .and_then(Value::as_bool),
            Some(true)
        );
        assert!(dir.path().join(".packet28/packet-cache-v2.bin").exists());
    }

    #[test]
    fn governed_cache_reuses_entries_for_same_policy_content_across_paths() {
        let dir = tempdir().unwrap();
        let persist = PersistConfig::new(dir.path().to_path_buf());
        let config_a = dir.path().join("policy-a.yaml");
        let config_b = dir.path().join("policy-b.yaml");
        write_policy_file(&config_a, &["diffy"], &[]);
        write_policy_file(&config_b, &["diffy"], &[]);

        let calls = Arc::new(AtomicU64::new(0));
        let calls_ref = calls.clone();
        let mut kernel = Kernel::with_v1_reducers_and_persistence(persist);
        kernel.register_reducer("count.reducer", move |_ctx, _packets| {
            calls_ref.fetch_add(1, Ordering::Relaxed);
            Ok(ReducerResult {
                output_packets: vec![KernelPacket::from_value(
                    json!({
                        "tool": "diffy",
                        "reducer": "analyze",
                        "paths": ["src/lib.rs"],
                        "payload": {"message": "ok"},
                    }),
                    None,
                )],
                metadata: json!({"source":"reducer"}),
            })
        });

        let mut request = KernelRequest {
            target: "count.reducer".to_string(),
            reducer_input: json!({"task":"governed-cache"}),
            policy_context: json!({
                "config_path": config_a.to_string_lossy().to_string()
            }),
            ..KernelRequest::default()
        };
        let first = kernel.execute(request.clone()).unwrap();
        assert_eq!(
            first
                .metadata
                .get("cache")
                .and_then(|v| v.get("hit"))
                .and_then(Value::as_bool),
            Some(false)
        );
        assert!(first.audit.governance.enabled);

        request.policy_context = json!({
            "config_path": config_b.to_string_lossy().to_string()
        });
        let second = kernel.execute(request).unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            second
                .metadata
                .get("cache")
                .and_then(|v| v.get("hit"))
                .and_then(Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn governed_cache_misses_when_policy_content_changes() {
        let dir = tempdir().unwrap();
        let persist = PersistConfig::new(dir.path().to_path_buf());
        let config_path = dir.path().join("policy.yaml");
        write_policy_file(&config_path, &["diffy"], &[]);

        let calls = Arc::new(AtomicU64::new(0));
        let calls_ref = calls.clone();
        let mut kernel = Kernel::with_v1_reducers_and_persistence(persist);
        kernel.register_reducer("count.reducer", move |_ctx, _packets| {
            calls_ref.fetch_add(1, Ordering::Relaxed);
            Ok(ReducerResult {
                output_packets: vec![KernelPacket::from_value(
                    json!({
                        "tool": "diffy",
                        "reducer": "analyze",
                        "paths": ["src/lib.rs"],
                        "payload": {"message": "ok"},
                    }),
                    None,
                )],
                metadata: json!({"source":"reducer"}),
            })
        });

        let request = KernelRequest {
            target: "count.reducer".to_string(),
            reducer_input: json!({"task":"governed-cache"}),
            policy_context: json!({
                "config_path": config_path.to_string_lossy().to_string()
            }),
            ..KernelRequest::default()
        };
        let first = kernel.execute(request.clone()).unwrap();
        assert_eq!(
            first
                .metadata
                .get("cache")
                .and_then(|v| v.get("hit"))
                .and_then(Value::as_bool),
            Some(false)
        );

        std::fs::write(
            &config_path,
            r#"
version: 1
policy:
  allowed_tools: ["diffy"]
  allowed_reducers: []
  paths:
    include: ["src/**"]
    exclude: []
  budgets:
    token_cap: 9000
    runtime_ms_cap: 2000
  redaction:
    forbidden_patterns: []
"#,
        )
        .unwrap();

        let second = kernel.execute(request).unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert_eq!(
            second
                .metadata
                .get("cache")
                .and_then(|v| v.get("hit"))
                .and_then(Value::as_bool),
            Some(false)
        );
    }

    #[test]
    fn governed_cache_hit_rechecks_output_policy_audits() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("policy.yaml");
        std::fs::write(
            &config_path,
            r#"
version: 1
policy:
  paths:
    include: ["src/**"]
    exclude: []
  redaction:
    forbidden_patterns: ["secret123"]
"#,
        )
        .unwrap();

        let mut kernel = Kernel::new();
        kernel.register_reducer("count.reducer", |_ctx, _packets| {
            Ok(ReducerResult {
                output_packets: vec![KernelPacket::from_value(json!({"ok": true}), None)],
                metadata: Value::Null,
            })
        });

        let request = KernelRequest {
            target: "count.reducer".to_string(),
            policy_context: json!({
                "config_path": config_path.to_string_lossy().to_string()
            }),
            ..KernelRequest::default()
        };
        let policy_guard = load_policy_guard(&request.policy_context).unwrap().unwrap();
        let cache_input = cache_input_for_request(&request, &request.target, Some(&policy_guard));
        let mut hooks = NoopDeltaReuseHooks;

        let lookup = {
            let cache = kernel.memory.lock().unwrap();
            cache.lookup_with_hooks(&request.target, &cache_input, &mut hooks)
        };
        {
            let mut cache = kernel.memory.lock().unwrap();
            cache.put_with_hooks(
                &request.target,
                &lookup,
                vec![CachePacket {
                    packet_id: Some("cached-bad".to_string()),
                    body: json!({
                        "tool": "diffy",
                        "reducer": "analyze",
                        "paths": ["src/lib.rs"],
                        "payload": {"secret": "secret123"},
                    }),
                    token_usage: None,
                    runtime_ms: None,
                    metadata: Value::Null,
                }],
                Value::Null,
                &mut hooks,
            );
        }

        let err = kernel.execute(request).unwrap_err();
        assert!(matches!(err, KernelError::PolicyViolation { .. }));
    }

    #[test]
    fn executes_sequence_in_dependency_order() {
        let mut kernel = Kernel::new();
        kernel.register_reducer("step.a", |_ctx, _packets| {
            Ok(ReducerResult {
                output_packets: vec![KernelPacket::from_value(json!({"step":"a"}), None)],
                metadata: Value::Null,
            })
        });
        kernel.register_reducer("step.b", |_ctx, _packets| {
            Ok(ReducerResult {
                output_packets: vec![KernelPacket::from_value(json!({"step":"b"}), None)],
                metadata: Value::Null,
            })
        });

        let response = kernel
            .execute_sequence(KernelSequenceRequest {
                budget: ExecutionBudget {
                    token_cap: Some(100),
                    byte_cap: None,
                    runtime_ms_cap: None,
                },
                reactive: ReactiveSequenceConfig::default(),
                steps: vec![
                    KernelStepRequest {
                        id: "b".to_string(),
                        target: "step.b".to_string(),
                        depends_on: vec!["a".to_string()],
                        input_packets: vec![],
                        ..KernelStepRequest::default()
                    },
                    KernelStepRequest {
                        id: "a".to_string(),
                        target: "step.a".to_string(),
                        depends_on: vec![],
                        input_packets: vec![],
                        ..KernelStepRequest::default()
                    },
                ],
            })
            .unwrap();

        assert_eq!(response.scheduled, vec!["a".to_string(), "b".to_string()]);
        assert!(response.skipped.is_empty());
    }

    #[test]
    fn sequence_autofills_missing_step_ids_and_resolves_dependencies() {
        let mut kernel = Kernel::new();
        kernel.register_reducer("step.a", |_ctx, _packets| Ok(ReducerResult::default()));
        kernel.register_reducer("step.b", |_ctx, _packets| Ok(ReducerResult::default()));

        let response = kernel
            .execute_sequence(KernelSequenceRequest {
                budget: ExecutionBudget::default(),
                reactive: ReactiveSequenceConfig::default(),
                steps: vec![
                    KernelStepRequest {
                        target: "step.a".to_string(),
                        ..KernelStepRequest::default()
                    },
                    KernelStepRequest {
                        id: " custom ".to_string(),
                        target: "step.b".to_string(),
                        depends_on: vec!["step-a-0".to_string()],
                        ..KernelStepRequest::default()
                    },
                ],
            })
            .unwrap();

        assert_eq!(
            response.scheduled,
            vec!["step-a-0".to_string(), "custom".to_string()]
        );
        assert_eq!(response.step_results[0].id, "step-a-0");
        assert_eq!(response.step_results[1].id, "custom");
    }

    #[test]
    fn sequence_rejects_empty_targets() {
        let kernel = Kernel::new();

        let err = kernel
            .execute_sequence(KernelSequenceRequest {
                budget: ExecutionBudget::default(),
                reactive: ReactiveSequenceConfig::default(),
                steps: vec![KernelStepRequest::default()],
            })
            .unwrap_err();

        assert!(
            matches!(err, KernelError::InvalidRequest { detail } if detail == "sequence step 0 target cannot be empty")
        );
    }

    #[test]
    fn sequence_rejects_duplicate_resolved_ids() {
        let kernel = Kernel::new();

        let err = kernel
            .execute_sequence(KernelSequenceRequest {
                budget: ExecutionBudget::default(),
                reactive: ReactiveSequenceConfig::default(),
                steps: vec![
                    KernelStepRequest {
                        id: "step-a-1".to_string(),
                        target: "step.a".to_string(),
                        ..KernelStepRequest::default()
                    },
                    KernelStepRequest {
                        target: "step.a".to_string(),
                        ..KernelStepRequest::default()
                    },
                ],
            })
            .unwrap_err();

        assert!(
            matches!(err, KernelError::InvalidRequest { detail } if detail == "sequence step id 'step-a-1' must be unique")
        );
    }

    #[test]
    fn sequence_respects_scheduler_budget_cutoff() {
        let mut kernel = Kernel::new();
        kernel.register_reducer("step.a", |_ctx, _packets| Ok(ReducerResult::default()));
        kernel.register_reducer("step.b", |_ctx, _packets| Ok(ReducerResult::default()));

        let packet = KernelPacket {
            body: json!({"size":"large"}),
            token_usage: Some(90),
            ..KernelPacket::default()
        };
        let response = kernel
            .execute_sequence(KernelSequenceRequest {
                budget: ExecutionBudget {
                    token_cap: Some(100),
                    byte_cap: None,
                    runtime_ms_cap: None,
                },
                reactive: ReactiveSequenceConfig::default(),
                steps: vec![
                    KernelStepRequest {
                        id: "a".to_string(),
                        target: "step.a".to_string(),
                        input_packets: vec![packet.clone()],
                        ..KernelStepRequest::default()
                    },
                    KernelStepRequest {
                        id: "b".to_string(),
                        target: "step.b".to_string(),
                        input_packets: vec![packet],
                        ..KernelStepRequest::default()
                    },
                ],
            })
            .unwrap();

        assert!(response.budget_exhausted);
        assert_eq!(response.scheduled, vec!["a".to_string()]);
        assert_eq!(response.skipped, vec!["b".to_string()]);
    }

    #[test]
    fn sequence_skips_dependent_step_after_failure() {
        let mut kernel = Kernel::new();
        kernel.register_reducer("step.fail", |_ctx, _packets| {
            Err(KernelError::ReducerFailed {
                target: "step.fail".to_string(),
                detail: "boom".to_string(),
            })
        });
        kernel.register_reducer("step.after", |_ctx, _packets| Ok(ReducerResult::default()));

        let response = kernel
            .execute_sequence(KernelSequenceRequest {
                budget: ExecutionBudget::default(),
                reactive: ReactiveSequenceConfig::default(),
                steps: vec![
                    KernelStepRequest {
                        id: "fail".to_string(),
                        target: "step.fail".to_string(),
                        ..KernelStepRequest::default()
                    },
                    KernelStepRequest {
                        id: "after".to_string(),
                        target: "step.after".to_string(),
                        depends_on: vec!["fail".to_string()],
                        ..KernelStepRequest::default()
                    },
                ],
            })
            .unwrap();

        let after = response
            .step_results
            .iter()
            .find(|step| step.id == "after")
            .unwrap();
        assert_eq!(after.status, "skipped");
    }

    #[test]
    fn reactive_sequence_cancels_completed_steps_and_releases_dependencies() {
        let kernel = Kernel::with_v1_reducers();
        kernel
            .execute(KernelRequest {
                target: "agenty.state.write".to_string(),
                reducer_input: json!({
                    "task_id": "task-reactive",
                    "event_id": "evt-1",
                    "occurred_at_unix": 1,
                    "actor": "agent",
                    "kind": "step_completed",
                    "data": {
                        "type": "step_completed",
                        "step_id": "map-step"
                    }
                }),
                ..KernelRequest::default()
            })
            .unwrap();

        let mut kernel = kernel;
        kernel.register_reducer("step.noop", |_ctx, _packets| Ok(ReducerResult::default()));

        let response = kernel
            .execute_sequence(KernelSequenceRequest {
                budget: ExecutionBudget::default(),
                reactive: ReactiveSequenceConfig {
                    enabled: true,
                    task_id: Some("task-reactive".to_string()),
                    append_focused_map: false,
                    mode: ReactiveReplanMode::Basic,
                },
                steps: vec![
                    KernelStepRequest {
                        id: "map-step".to_string(),
                        target: "step.noop".to_string(),
                        ..KernelStepRequest::default()
                    },
                    KernelStepRequest {
                        id: "final-step".to_string(),
                        target: "step.noop".to_string(),
                        depends_on: vec!["map-step".to_string()],
                        ..KernelStepRequest::default()
                    },
                ],
            })
            .unwrap();

        assert_eq!(response.scheduled, vec!["final-step".to_string()]);
        assert!(response.skipped.contains(&"map-step".to_string()));
        assert!(response
            .metadata
            .get("reactive")
            .and_then(|reactive| reactive.get("replans"))
            .and_then(Value::as_array)
            .is_some_and(|replans| !replans.is_empty()));
    }

    #[test]
    fn reactive_sequence_replaces_map_steps_after_focus_update() {
        let dir = tempdir().unwrap();
        setup_diff_repo(dir.path());

        let mut kernel = Kernel::with_v1_reducers();
        kernel.register_reducer("custom.focus", |ctx, _packets| {
            let event = suite_packet_core::AgentStateEventPayload {
                task_id: "task-focus".to_string(),
                event_id: "focus-1".to_string(),
                occurred_at_unix: 2,
                actor: "agent".to_string(),
                kind: suite_packet_core::AgentStateEventKind::FocusSet,
                paths: vec!["src/alpha.rs".to_string()],
                symbols: Vec::new(),
                data: suite_packet_core::AgentStateEventData::FocusSet { note: None },
            };
            let (_, packet) = build_agent_state_packet(&ctx.target, &event, "custom.focus")?;
            Ok(ReducerResult {
                output_packets: vec![packet],
                metadata: json!({"source":"custom.focus"}),
            })
        });

        let response = kernel
            .execute_sequence(KernelSequenceRequest {
                budget: ExecutionBudget::default(),
                reactive: ReactiveSequenceConfig {
                    enabled: true,
                    task_id: Some("task-focus".to_string()),
                    append_focused_map: false,
                    mode: ReactiveReplanMode::Basic,
                },
                steps: vec![
                    KernelStepRequest {
                        id: "focus".to_string(),
                        target: "custom.focus".to_string(),
                        ..KernelStepRequest::default()
                    },
                    KernelStepRequest {
                        id: "map".to_string(),
                        target: "mapy.repo".to_string(),
                        reducer_input: serde_json::to_value(mapy_core::RepoMapRequest {
                            repo_root: dir.path().to_string_lossy().to_string(),
                            focus_paths: Vec::new(),
                            focus_symbols: Vec::new(),
                            max_files: 10,
                            max_symbols: 20,
                            include_tests: false,
                        })
                        .unwrap(),
                        ..KernelStepRequest::default()
                    },
                ],
            })
            .unwrap();

        let map_response = response
            .step_results
            .iter()
            .find(|step| step.id == "map")
            .and_then(|step| step.response.as_ref())
            .unwrap();
        assert_eq!(
            map_response
                .metadata
                .get("focus_paths")
                .and_then(Value::as_array)
                .and_then(|paths| paths.first())
                .and_then(Value::as_str),
            Some("src/alpha.rs")
        );
    }

    #[test]
    fn reactive_mutations_can_append_focused_map_followup() {
        let original = vec![KernelStepRequest {
            id: "map".to_string(),
            target: "mapy.repo".to_string(),
            reducer_input: serde_json::to_value(mapy_core::RepoMapRequest {
                repo_root: ".".to_string(),
                focus_paths: Vec::new(),
                focus_symbols: Vec::new(),
                max_files: 10,
                max_symbols: 20,
                include_tests: false,
            })
            .unwrap(),
            ..KernelStepRequest::default()
        }];
        let remaining = vec![KernelStepRequest {
            id: "other".to_string(),
            target: "step.noop".to_string(),
            ..KernelStepRequest::default()
        }];
        let snapshot = suite_packet_core::AgentSnapshotPayload {
            task_id: "task-focus".to_string(),
            focus_paths: vec!["src/alpha.rs".to_string()],
            ..suite_packet_core::AgentSnapshotPayload::default()
        };

        let mutations = build_reactive_kernel_mutations(
            &remaining,
            &original,
            &snapshot,
            &BTreeSet::new(),
            ReactiveReplanMode::Basic,
            true,
            Some("other"),
        );

        assert!(mutations.iter().any(|mutation| matches!(
            mutation,
            KernelPlanMutation::Append { step, .. }
                if step.id == "map__reactive_focus"
                && step.depends_on == vec!["other".to_string()]
        )));
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
                "data": {
                    "type": "decision_added",
                    "decision_id": "d1",
                    "text": "Bug is in split()",
                    "supersedes": null
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
        assert_eq!(envelope.payload.event_count, 6);
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
        let focus_envelope: suite_packet_core::EnvelopeV1<
            suite_packet_core::AgentStateEventPayload,
        > = serde_json::from_value(focus_packet.body.clone()).unwrap();
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
        let snapshot_envelope: suite_packet_core::EnvelopeV1<
            suite_packet_core::AgentSnapshotPayload,
        > = serde_json::from_value(snapshot.output_packets[0].body.clone()).unwrap();
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
        let kernel =
            Kernel::with_v1_reducers_and_persistence(PersistConfig::new(dir.path().to_path_buf()));

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
            let mut cache = kernel.memory.lock().unwrap();
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
}
