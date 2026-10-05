use super::*;
use crate::savings_analytics::{record_run_savings, RunSavingsRecord};

fn task_artifact_dir(root: &Path, task_id: &str) -> PathBuf {
    packet28_daemon_protocol::paths::task_artifact_dir(
        root,
        &TaskStorageId::try_from(task_id).unwrap(),
    )
}

fn drift_payload(ok: bool, issue_count: u64) -> Value {
    let issues = if issue_count == 0 {
        Vec::new()
    } else {
        vec![serde_json::json!({
            "case_id": "cargo-failing-test-name",
            "kind": "missing_marker",
            "detail": "FAIL drift_marker"
        })]
    };
    serde_json::json!({
        "ok": ok,
        "case_count": 1,
        "issue_count": issue_count,
        "issues": issues,
        "summaries": [{
            "case_id": "cargo-failing-test-name",
            "family": "rust",
            "canonical_kind": "rust_test",
            "summary": "cargo test reported 0 passed and 1 failed"
        }]
    })
}

fn memory_lint_payload(ok: bool, issue_count: u64) -> Value {
    let issues = if issue_count == 0 {
        Vec::new()
    } else {
        vec![serde_json::json!({
            "memory_id": 1,
            "kind": "runtime_specific_memory",
            "detail": "mentions windsurf"
        })]
    };
    serde_json::json!({
        "ok": ok,
        "memory_count": 2,
        "issue_count": issue_count,
        "lint": {
            "memory_count": 2,
            "issue_count": issue_count,
            "issues": issues
        }
    })
}

fn anomaly(category: &str, severity: &str) -> ContextAnomaly {
    ContextAnomaly {
        category: category.to_string(),
        severity: severity.to_string(),
        signal: "fixture".to_string(),
        next_check: "Packet28 digest --json".to_string(),
        repair_hint: "fixture hint".to_string(),
    }
}

fn context_anomaly_payload(
    ok: bool,
    anomaly_count: u64,
    high_count: u64,
    hidden_categories: Vec<&str>,
) -> Value {
    serde_json::json!({
        "ok": ok,
        "anomaly_count": anomaly_count,
        "high_count": high_count,
        "hidden_categories": hidden_categories
    })
}

#[test]
fn reducer_drift_tile_reports_recurring_and_cleared_latest_failure() {
    let root = tempfile::tempdir().unwrap();
    record_reducer_drift_history(root.path(), &drift_payload(false, 1)).unwrap();
    record_reducer_drift_history(root.path(), &drift_payload(false, 1)).unwrap();
    record_reducer_drift_history(root.path(), &drift_payload(true, 0)).unwrap();

    let tile = reducer_drift_tile(root.path()).unwrap();

    assert_eq!(tile.run_count, 3);
    assert_eq!(tile.latest_status, "ready");
    assert_eq!(tile.latest_issue_count, 0);
    assert!(tile.latest_failing_families.is_empty());
    assert_eq!(tile.recurring_issue_kinds, vec!["missing_marker"]);
    assert!(serde_json::to_string(&tile).unwrap().len() < 768);
}

#[test]
fn memory_lint_tile_reports_recurring_and_cleared_latest_issue() {
    let root = tempfile::tempdir().unwrap();
    record_memory_lint_history(root.path(), &memory_lint_payload(false, 1)).unwrap();
    record_memory_lint_history(root.path(), &memory_lint_payload(false, 1)).unwrap();
    record_memory_lint_history(root.path(), &memory_lint_payload(true, 0)).unwrap();

    let tile = memory_lint_tile(root.path()).unwrap();

    assert_eq!(tile.run_count, 3);
    assert_eq!(tile.latest_status, "ready");
    assert_eq!(tile.latest_issue_count, 0);
    assert!(tile.latest_issue_kinds.is_empty());
    assert_eq!(tile.recurring_issue_kinds, vec!["runtime_specific_memory"]);
    assert!(serde_json::to_string(&tile).unwrap().len() < 768);
}

#[test]
fn context_anomaly_tile_reports_recurring_hidden_after_clean_latest() {
    let root = tempfile::tempdir().unwrap();
    record_context_anomaly_history(
        root.path(),
        &context_anomaly_payload(false, 3, 1, vec!["fallback_provenance"]),
    )
    .unwrap();
    record_context_anomaly_history(
        root.path(),
        &context_anomaly_payload(false, 3, 1, vec!["fallback_provenance"]),
    )
    .unwrap();
    record_context_anomaly_history(
        root.path(),
        &context_anomaly_payload(true, 0, 0, Vec::new()),
    )
    .unwrap();

    let tile = context_anomaly_tile(root.path(), None).unwrap();

    assert_eq!(tile.run_count, 3);
    assert_eq!(tile.latest_status, "ready");
    assert_eq!(tile.latest_anomaly_count, 0);
    assert_eq!(tile.latest_high_count, 0);
    assert!(tile.oldest_recurring_hidden_age_ms >= tile.latest_age_ms);
    assert!(tile.latest_hidden_categories.is_empty());
    assert_eq!(
        tile.recurring_hidden_categories,
        vec!["fallback_provenance"]
    );
    assert!(serde_json::to_string(&tile).unwrap().len() < 768);
}

#[test]
fn context_anomaly_age_summary_distinguishes_old_recurring_hidden() {
    let records = vec![
        ContextAnomalyHistoryRecord {
            created_at_unix_ms: 1_000,
            hidden_categories: vec!["fallback_provenance".to_string()],
            ..ContextAnomalyHistoryRecord::default()
        },
        ContextAnomalyHistoryRecord {
            created_at_unix_ms: 8_000,
            hidden_categories: vec!["fallback_provenance".to_string()],
            ..ContextAnomalyHistoryRecord::default()
        },
        ContextAnomalyHistoryRecord {
            created_at_unix_ms: 9_000,
            hidden_categories: Vec::new(),
            ..ContextAnomalyHistoryRecord::default()
        },
    ];

    let (latest_age_ms, oldest_recurring_hidden_age_ms) =
        context_anomaly_age_summary(&records, 10_000);

    assert_eq!(latest_age_ms, 1_000);
    assert_eq!(oldest_recurring_hidden_age_ms, 9_000);
}

#[test]
fn context_anomaly_tile_reads_checked_in_trend_fixture() {
    let root = tempfile::tempdir().unwrap();
    let fixture = include_str!("../tests/fixtures/context_anomalies/history.jsonl");
    let history_path = root
        .path()
        .join(".packet28")
        .join("context-anomaly-history.jsonl");
    std::fs::create_dir_all(history_path.parent().unwrap()).unwrap();
    std::fs::write(&history_path, fixture).unwrap();

    let tile = context_anomaly_tile(root.path(), Some(&history_path)).unwrap();

    assert!(fixture.len() < 512);
    assert_eq!(tile.run_count, 3);
    assert_eq!(tile.latest_status, "ready");
    assert!(tile.oldest_recurring_hidden_age_ms >= tile.latest_age_ms);
    assert!(tile.latest_hidden_categories.is_empty());
    assert_eq!(
        tile.recurring_hidden_categories,
        vec!["fallback_provenance"]
    );
}

#[test]
fn context_anomaly_digest_ranks_drift_and_memory_with_next_checks() {
    let root = tempfile::tempdir().unwrap();
    record_reducer_drift_history(root.path(), &drift_payload(false, 1)).unwrap();
    record_memory_lint_history(root.path(), &memory_lint_payload(false, 1)).unwrap();

    let digest = context_anomaly_digest(root.path()).unwrap();

    assert_eq!(digest.anomaly_count, 2);
    assert_eq!(digest.anomalies[0].category, "reducer_drift");
    assert_eq!(digest.anomalies[0].severity, "high");
    assert!(digest.anomalies[0].next_check.contains("reducer-drift"));
    assert!(digest.anomalies[0].repair_hint.contains("compact markers"));
    assert_eq!(digest.anomalies[1].category, "memory_lint");
    assert_eq!(digest.anomalies[1].severity, "high");
    assert!(digest.anomalies[1].next_check.contains("memory-lint"));
    assert!(digest.anomalies[1].repair_hint.contains("stale runtime"));
    assert!(serde_json::to_string(&digest).unwrap().len() < 1024);
}

#[test]
fn context_anomaly_digest_includes_medium_fallback_provenance() {
    let root = tempfile::tempdir().unwrap();
    record_reducer_drift_history(root.path(), &drift_payload(false, 1)).unwrap();
    record_run_savings(
        root.path(),
        &RunSavingsRecord {
            command: "p28 search --backend fff query".to_string(),
            cwd: root.path().display().to_string(),
            family: "search".to_string(),
            canonical_kind: "search".to_string(),
            exit_code: 0,
            raw_est_tokens: 1200,
            reduced_est_tokens: 200,
            estimate_scope: None,
            savings_percent: 83.0,
            fallback_reason: Some("fff auto preferred backend failed: launch error".to_string()),
            failure_fingerprint: None,
            changed_paths: Vec::new(),
            timestamp_unix_ms: 1,
        },
    )
    .unwrap();

    let digest = context_anomaly_digest(root.path()).unwrap();

    assert_eq!(digest.anomaly_count, 2);
    assert_eq!(digest.anomalies[0].category, "reducer_drift");
    assert_eq!(digest.anomalies[0].severity, "high");
    assert_eq!(digest.anomalies[1].category, "fallback_provenance");
    assert_eq!(digest.anomalies[1].severity, "medium");
    assert!(digest.anomalies[1].next_check.contains("gain --failures"));
    assert!(digest.anomalies[1]
        .repair_hint
        .contains("fallback provenance"));
    assert!(serde_json::to_string(&digest).unwrap().len() < 1024);
}

#[test]
fn context_anomaly_digest_includes_changed_path_reread_signal() {
    let root = tempfile::tempdir().unwrap();
    record_reducer_drift_history(root.path(), &drift_payload(false, 1)).unwrap();
    record_run_savings(
        root.path(),
        &RunSavingsRecord {
            command: "Packet28 run -- cargo test".to_string(),
            cwd: root.path().display().to_string(),
            family: "rust".to_string(),
            canonical_kind: "cargo_test".to_string(),
            exit_code: 0,
            raw_est_tokens: 900,
            reduced_est_tokens: 200,
            estimate_scope: None,
            savings_percent: 77.0,
            fallback_reason: None,
            failure_fingerprint: None,
            changed_paths: vec!["src/lib.rs".to_string()],
            timestamp_unix_ms: 1,
        },
    )
    .unwrap();

    let digest = context_anomaly_digest(root.path()).unwrap();

    assert_eq!(digest.anomaly_count, 2);
    assert_eq!(digest.anomalies[0].category, "reducer_drift");
    assert_eq!(digest.anomalies[0].severity, "high");
    assert_eq!(digest.anomalies[1].category, "stale_changed_paths");
    assert_eq!(digest.anomalies[1].severity, "medium");
    assert!(digest.anomalies[1].next_check.contains("src/lib.rs"));
    assert!(digest.anomalies[1]
        .repair_hint
        .contains("reread changed paths"));
    assert!(serde_json::to_string(&digest).unwrap().len() < 1024);
}

#[test]
fn context_anomaly_finalizer_caps_mediums_after_high_anomalies() {
    let mut anomalies = vec![
        anomaly("fallback_provenance", "medium"),
        anomaly("stale_changed_paths", "medium"),
        anomaly("memory_lint", "high"),
        anomaly("extra_medium_a", "medium"),
        anomaly("reducer_drift", "high"),
        anomaly("extra_medium_b", "medium"),
    ];

    let hidden = finalize_context_anomalies(&mut anomalies);

    assert_eq!(anomalies.len(), MAX_CONTEXT_ANOMALIES);
    assert_eq!(anomalies[0].category, "reducer_drift");
    assert_eq!(anomalies[1].category, "memory_lint");
    assert!(anomalies
        .iter()
        .take(2)
        .all(|anomaly| anomaly.severity == "high"));
    assert!(anomalies
        .iter()
        .all(|anomaly| anomaly.category != "extra_medium_b"));
    assert_eq!(
        hidden
            .iter()
            .map(|anomaly| anomaly.category.as_str())
            .collect::<Vec<_>>(),
        vec!["fallback_provenance", "extra_medium_a", "extra_medium_b"]
    );
    assert!(serde_json::to_string(&anomalies).unwrap().len() < 1024);
}

#[test]
fn context_anomaly_digest_reports_hidden_categories_after_cap() {
    let root = tempfile::tempdir().unwrap();
    record_reducer_drift_history(root.path(), &drift_payload(false, 1)).unwrap();
    record_memory_lint_history(root.path(), &memory_lint_payload(false, 1)).unwrap();
    let handoff_path = task_artifact_dir(root.path(), "task-hidden-categories")
        .join("versions")
        .join("ctx-hidden.json");
    std::fs::create_dir_all(handoff_path.parent().unwrap()).unwrap();
    std::fs::write(
        &handoff_path,
        serde_json::to_vec(&serde_json::json!({
            "brief": "Review docs/missing-handoff-path.md before handoff.",
            "next_action_summary": "prepare the handoff packet"
        }))
        .unwrap(),
    )
    .unwrap();
    for index in 0..3 {
        record_run_savings(
            root.path(),
            &RunSavingsRecord {
                command: format!("Packet28 run -- cargo test {index}"),
                cwd: root.path().display().to_string(),
                family: "rust".to_string(),
                canonical_kind: "cargo_test".to_string(),
                exit_code: 0,
                raw_est_tokens: 900,
                reduced_est_tokens: 200,
                estimate_scope: None,
                savings_percent: 77.0,
                fallback_reason: if index == 0 {
                    Some("unsupported_family".to_string())
                } else {
                    None
                },
                failure_fingerprint: None,
                changed_paths: vec![format!("src/lib{index}.rs")],
                timestamp_unix_ms: index + 1,
            },
        )
        .unwrap();
    }

    let digest = context_anomaly_digest(root.path()).unwrap();

    assert_eq!(digest.anomaly_count, MAX_CONTEXT_ANOMALIES);
    assert_eq!(digest.truncated_count, 2);
    assert_eq!(
        digest.hidden_categories,
        vec!["stale_changed_paths", "fallback_provenance"]
    );
    assert!(digest
        .hidden_samples
        .iter()
        .any(|sample| sample.category == "fallback_provenance"
            && sample.signal.contains("unsupported_family")));
    assert_eq!(digest.anomalies[0].category, "reducer_drift");
    assert_eq!(digest.anomalies[1].category, "memory_lint");
    assert!(serde_json::to_string(&digest).unwrap().len() < 1024);
}

#[test]
fn context_hidden_samples_truncate_long_signals() {
    let long_reason = format!(
        "recent_fallbacks=1 latest_reason={} source=run-savings",
        "x".repeat(240)
    );
    let hidden = vec![ContextAnomaly {
        category: "fallback_provenance".to_string(),
        severity: "medium".to_string(),
        signal: long_reason,
        next_check: "Packet28 gain --failures".to_string(),
        repair_hint: "inspect fallback provenance before treating output as success".to_string(),
    }];

    let samples = context_hidden_samples(&hidden);

    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].category, "fallback_provenance");
    assert!(samples[0].signal.len() <= 120);
    assert!(samples[0].signal.contains("recent_fallbacks=1"));
    assert!(samples[0].signal.contains("latest_reason="));
    assert!(
        serde_json::to_string(&samples).unwrap().len() < 512,
        "sample summary should stay compact"
    );
}

#[test]
fn context_hidden_sample_summary_escapes_pair_delimiters() {
    let samples = vec![ContextHiddenSample {
        category: "fallback_provenance".to_string(),
        signal: "recent_fallbacks=1 latest_reason=alpha=beta;next=retry\nsource=run-savings"
            .to_string(),
    }];

    let summary = context_hidden_sample_summary(&samples);

    assert_eq!(summary.split(';').count(), 1);
    assert!(summary.starts_with("fallback_provenance=recent_fallbacks=1"));
    assert!(summary.contains("alpha=beta%3Bnext=retry%0Asource=run-savings"));
    assert!(!summary.contains(";next=retry"));
    assert!(summary.len() < 256);
}

#[test]
fn context_hidden_sample_summary_matches_delimiter_fixture() {
    let samples = serde_json::from_str::<Vec<ContextHiddenSample>>(include_str!(
        "../tests/fixtures/context_anomalies/hidden-samples-delimiters.json"
    ))
    .unwrap();
    let expected =
        include_str!("../tests/fixtures/context_anomalies/hidden-samples-delimiters.summary")
            .trim();

    let summary = context_hidden_sample_summary(&samples);

    assert_eq!(summary, expected);
    assert!(summary.len() < 256);
}

#[test]
fn context_anomaly_tile_reports_hidden_category_drilldown_sample() {
    let root = tempfile::tempdir().unwrap();
    let payload = serde_json::json!({
        "ok": true,
        "anomaly_count": 3,
        "high_count": 1,
        "hidden_categories": ["fallback_provenance"],
        "hidden_samples": [{
            "category": "fallback_provenance",
            "signal": "recent_fallbacks=1 latest_reason=fff failed"
        }]
    });
    record_context_anomaly_history(root.path(), &payload).unwrap();
    record_context_anomaly_history(root.path(), &payload).unwrap();

    let tile = context_anomaly_tile(root.path(), None).unwrap();

    assert_eq!(tile.recurring_hidden_samples.len(), 1);
    assert_eq!(
        tile.recurring_hidden_samples[0].category,
        "fallback_provenance"
    );
    assert!(tile.recurring_hidden_samples[0]
        .signal
        .contains("recent_fallbacks=1"));
    assert!(
        serde_json::to_string(&tile.recurring_hidden_samples)
            .unwrap()
            .len()
            < 512
    );
}

#[test]
fn context_anomaly_tile_uses_latest_hidden_category_sample() {
    let root = tempfile::tempdir().unwrap();
    let older_payload = serde_json::json!({
        "ok": true,
        "anomaly_count": 3,
        "high_count": 1,
        "hidden_categories": ["fallback_provenance"],
        "hidden_samples": [{
            "category": "fallback_provenance",
            "signal": "recent_fallbacks=1 latest_reason=older unsupported family"
        }]
    });
    let newer_payload = serde_json::json!({
        "ok": true,
        "anomaly_count": 3,
        "high_count": 1,
        "hidden_categories": ["fallback_provenance"],
        "hidden_samples": [{
            "category": "fallback_provenance",
            "signal": "recent_fallbacks=1 latest_reason=newer fallback sample"
        }]
    });
    record_context_anomaly_history(root.path(), &older_payload).unwrap();
    record_context_anomaly_history(root.path(), &newer_payload).unwrap();

    let tile = context_anomaly_tile(root.path(), None).unwrap();

    assert_eq!(tile.recurring_hidden_samples.len(), 1);
    assert_eq!(
        tile.recurring_hidden_samples[0].category,
        "fallback_provenance"
    );
    assert!(tile.recurring_hidden_samples[0]
        .signal
        .contains("newer fallback sample"));
    assert!(!tile.recurring_hidden_samples[0]
        .signal
        .contains("older unsupported family"));
    assert!(
        serde_json::to_string(&tile.recurring_hidden_samples)
            .unwrap()
            .len()
            < 512
    );
}

#[test]
fn context_anomaly_digest_reports_recurring_hidden_history() {
    let root = tempfile::tempdir().unwrap();
    let payload = context_anomaly_payload(true, 3, 1, vec!["fallback_provenance"]);
    record_context_anomaly_history(root.path(), &payload).unwrap();
    record_context_anomaly_history(root.path(), &payload).unwrap();

    let digest = context_anomaly_digest(root.path()).unwrap();

    assert_eq!(digest.anomaly_count, 1);
    assert_eq!(digest.truncated_count, 0);
    assert_eq!(digest.anomalies[0].category, "context_anomaly_trend");
    assert_eq!(digest.anomalies[0].severity, "medium");
    assert!(digest.anomalies[0].signal.contains("fallback_provenance"));
    assert!(serde_json::to_string(&digest).unwrap().len() < 1024);
}
