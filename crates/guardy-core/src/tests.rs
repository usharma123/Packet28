use super::*;
use std::fs;
use tempfile::TempDir;

#[test]
fn validate_config_rejects_invalid_schema_and_rules() {
    let yaml = r#"
version: 2
policy:
  tools:
    allowlist: [""]
  allowed_tools: [""]
  paths:
    include: ["[broken"]
  token_budget:
    cap: 0
  budgets:
    token_cap: 0
    runtime_ms_cap: 0
  runtime_budget:
    cap_ms: 0
  redaction:
    forbidden_patterns: ["("]
"#;

    let result = validate_config_str(yaml);
    assert!(!result.valid);
    assert!(result
        .errors
        .iter()
        .any(|e| e.contains("unsupported policy version")));
    assert!(result
        .errors
        .iter()
        .any(|e| e.contains("allowed_tools[0] cannot be empty")));
    assert!(result.errors.iter().any(|e| e.contains("invalid glob")));
    assert!(result
        .errors
        .iter()
        .any(|e| e.contains("policy.token_budget.cap")));
    assert!(result.errors.iter().any(|e| e.contains("token_cap")));
    assert!(result
        .errors
        .iter()
        .any(|e| e.contains("policy.runtime_budget.cap_ms")));
    assert!(result.errors.iter().any(|e| e.contains("runtime_ms_cap")));
    assert!(result.errors.iter().any(|e| e.contains("invalid regex")));
}

#[test]
fn validate_config_accepts_canonical_policy_shape() {
    let yaml = r#"
version: 1
policy:
  tools:
    allowlist: ["diffy"]
  reducers:
    allowlist: ["analyze"]
  paths:
    include: ["src/**"]
    exclude: []
  token_budget:
    cap: 300
  runtime_budget:
    cap_ms: 2000
  redaction:
    forbidden_patterns: ["(?i)secret"]
  human_review:
    required: true
    on_policy_violation: true
    on_budget_violation: false
    on_redaction_violation: true
"#;

    let result = validate_config_str(yaml);
    assert!(result.valid);

    let config = parse_context_strict(yaml).unwrap();
    assert_eq!(
        config.policy.effective_allowed_tools(),
        vec!["diffy".to_string()]
    );
    assert_eq!(
        config.policy.effective_allowed_reducers(),
        vec!["analyze".to_string()]
    );
    assert_eq!(config.policy.effective_token_cap(), Some(300));
    assert_eq!(config.policy.effective_runtime_ms_cap(), Some(2000));
    assert!(config.policy.human_review.required);
}

#[test]
fn validate_config_accepts_legacy_policy_aliases() {
    let yaml = r#"
version: 1
policy:
  tool_allowlist: ["diffy"]
  reducer_allowlist: ["analyze"]
  path_rules:
    include: ["src/**"]
    exclude: []
  budget_rules:
    token_cap: 300
    runtime_ms_cap: 2000
  redaction_rules:
    forbidden_patterns: ["(?i)secret"]
  human_review:
    required: true
    on_policy_violation: true
    on_budget_violation: false
    on_redaction_violation: true
"#;

    let result = validate_config_str(yaml);
    assert!(result.valid);

    let config = parse_context_strict(yaml).unwrap();
    assert_eq!(
        config.policy.effective_allowed_tools(),
        vec!["diffy".to_string()]
    );
    assert_eq!(
        config.policy.effective_allowed_reducers(),
        vec!["analyze".to_string()]
    );
    assert_eq!(config.policy.effective_token_cap(), Some(300));
    assert_eq!(config.policy.effective_runtime_ms_cap(), Some(2000));
    assert!(config.policy.human_review.required);
    assert!(config.policy.human_review.on_policy_violation);
    assert!(!config.policy.human_review.on_budget_violation);
    assert!(config.policy.human_review.on_redaction_violation);
}

#[test]
fn validate_config_rejects_conflicting_canonical_and_legacy_fields() {
    let yaml = r#"
version: 1
policy:
  tools:
    allowlist: ["diffy"]
  allowed_tools: ["covy"]
  reducers:
    allowlist: ["analyze"]
  allowed_reducers: ["merge"]
  paths:
    include: ["src/**"]
    exclude: []
  token_budget:
    cap: 200
  budgets:
    token_cap: 300
    runtime_ms_cap: 1200
  runtime_budget:
    cap_ms: 1000
  redaction:
    forbidden_patterns: []
"#;

    let result = validate_config_str(yaml);
    assert!(!result.valid);
    assert!(result
        .errors
        .iter()
        .any(|e| e.contains("policy.tools.allowlist conflicts with policy.allowed_tools")));
    assert!(result.errors.iter().any(|e| {
        e.contains("policy.reducers.allowlist conflicts with policy.allowed_reducers")
    }));
    assert!(result
        .errors
        .iter()
        .any(|e| e.contains("policy.token_budget.cap conflicts with policy.budgets.token_cap")));
    assert!(result.errors.iter().any(|e| {
        e.contains("policy.runtime_budget.cap_ms conflicts with policy.budgets.runtime_ms_cap")
    }));
}

#[test]
fn check_packet_reports_policy_violations() {
    let yaml = r#"
version: 1
policy:
  allowed_tools: ["covy"]
  allowed_reducers: ["merge"]
  paths:
    include: ["src/**"]
    exclude: ["src/private/**"]
  budgets:
    token_cap: 100
    runtime_ms_cap: 500
  redaction:
    forbidden_patterns: ["(?i)password"]
"#;

    let config = parse_context_strict(yaml).unwrap();
    let packet: GuardPacket = serde_json::from_str(
        r#"{
  "tool": "unknown-tool",
  "reducer": "bad-reducer",
  "paths": ["src/private/secret.txt"],
  "token_usage": 130,
  "runtime_ms": 800,
  "payload": {"note": "password=123"}
}"#,
    )
    .unwrap();

    let result = check_packet(&config, &packet);
    assert!(!result.passed);
    assert!(result.findings.iter().any(|f| f.rule == "allowed_tools"));
    assert!(result.findings.iter().any(|f| f.rule == "allowed_reducers"));
    assert!(result.findings.iter().any(|f| f.rule == "path_exclude"));
    assert!(result.findings.iter().any(|f| f.rule == "token_cap"));
    assert!(result.findings.iter().any(|f| f.rule == "runtime_ms_cap"));
    assert!(result.findings.iter().any(|f| f.rule == "redaction"));
}

#[test]
fn check_packet_passes_for_compliant_input() {
    let yaml = r#"
version: 1
policy:
  allowed_tools: ["covy", "diffy"]
  allowed_reducers: ["merge"]
  paths:
    include: ["src/**"]
    exclude: ["src/private/**"]
  budgets:
    token_cap: 200
    runtime_ms_cap: 1000
  redaction:
    forbidden_patterns: ["(?i)secret"]
"#;

    let config = parse_context_strict(yaml).unwrap();
    let packet: GuardPacket = serde_json::from_str(
        r#"{
  "tool": "covy",
  "reducer": "merge",
  "paths": ["src/lib.rs"],
  "token_usage": 120,
  "runtime_ms": 300,
  "payload": {"note": "all clear"}
}"#,
    )
    .unwrap();

    let result = check_packet(&config, &packet);
    assert!(result.passed);
    assert!(result.findings.is_empty());
    assert_eq!(result.totals.tools_seen, 1);
    assert_eq!(result.totals.reducers_seen, 1);
    assert_eq!(result.totals.paths_seen, 1);
}

#[test]
fn file_based_validate_and_check_roundtrip() {
    let dir = TempDir::new().unwrap();
    let config_path = dir.path().join("context.yaml");
    let packet_path = dir.path().join("packet.json");

    fs::write(
        &config_path,
        r#"
version: 1
policy:
  allowed_tools: ["covy"]
  allowed_reducers: []
  paths:
    include: ["src/**"]
    exclude: []
  budgets:
    token_cap: 100
    runtime_ms_cap: 400
  redaction:
    forbidden_patterns: []
"#,
    )
    .unwrap();

    fs::write(
        &packet_path,
        r#"{
  "tool": "covy",
  "paths": ["src/main.rs"],
  "token_usage": 10,
  "runtime_ms": 20,
  "payload": {"ok": "yes"}
}"#,
    )
    .unwrap();

    let validate = validate_config_file(&config_path).unwrap();
    assert!(validate.valid);

    let audit = check_packet_file(&packet_path, &config_path).unwrap();
    assert!(audit.passed);
}

#[test]
fn check_packet_paths_from_top_level_refs() {
    let yaml = r#"
version: 1
policy:
  allowed_tools: ["mapy"]
  allowed_reducers: []
  paths:
    include: ["src/**"]
    exclude: ["src/private/**"]
  budgets:
    token_cap: 1000
    runtime_ms_cap: 2000
  redaction:
    forbidden_patterns: []
"#;
    let config = parse_context_strict(yaml).unwrap();

    let packet: GuardPacket = serde_json::from_str(
        r#"{
  "tool": "mapy",
  "files": [
    {"path": "src/lib.rs"},
    {"path": "src/private/secret.rs"}
  ],
  "payload": {}
}"#,
    )
    .unwrap();

    let result = check_packet(&config, &packet);
    assert!(!result.passed);
    assert!(result.findings.iter().any(|f| f.rule == "path_exclude"));
}

#[test]
fn check_packet_scans_files_and_symbols_for_redaction() {
    let yaml = r#"
version: 1
policy:
  paths:
    include: ["**"]
    exclude: []
  redaction:
    forbidden_patterns: ["secret123"]
"#;
    let config = parse_context_strict(yaml).unwrap();

    let packet: GuardPacket = serde_json::from_str(
        r#"{
  "files": [{"path": "src/secret123.txt"}],
  "symbols": [{"name": "ok", "file": "src/main.rs"}],
  "payload": {}
}"#,
    )
    .unwrap();

    let result = check_packet(&config, &packet);
    assert!(!result.passed);
    assert!(result
        .findings
        .iter()
        .any(|f| f.rule == "redaction" && f.subject == "packet.files[0].path"));
}

#[test]
fn check_packet_scans_summary_for_redaction() {
    let yaml = r#"
version: 1
policy:
  paths:
    include: ["**"]
    exclude: []
  redaction:
    forbidden_patterns: ["secret123"]
"#;
    let config = parse_context_strict(yaml).unwrap();

    let packet: GuardPacket = serde_json::from_str(
        r#"{
  "summary": "contains secret123",
  "payload": {}
}"#,
    )
    .unwrap();

    let result = check_packet(&config, &packet);
    assert!(!result.passed);
    assert!(result
        .findings
        .iter()
        .any(|f| f.rule == "redaction" && f.subject == "packet.summary"));
}

#[test]
fn check_packet_scans_provenance_for_redaction() {
    let yaml = r#"
version: 1
policy:
  paths:
    include: ["**"]
    exclude: []
  redaction:
    forbidden_patterns: ["secret123"]
"#;
    let config = parse_context_strict(yaml).unwrap();

    let packet: GuardPacket = serde_json::from_str(
        r#"{
  "provenance": {
    "inputs": ["my_secret123_input"],
    "generated_at_unix": 1
  },
  "payload": {}
}"#,
    )
    .unwrap();

    let result = check_packet(&config, &packet);
    assert!(!result.passed);
    assert!(result
        .findings
        .iter()
        .any(|f| f.rule == "redaction" && f.subject == "packet.provenance.inputs[0]"));
}
