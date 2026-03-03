use std::path::PathBuf;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

#[test]
fn valid_context_fixture_passes_validation() {
    let result = guardy_core::validate_config_file(&fixture_path("valid_context.yaml")).unwrap();
    assert!(result.valid);
    assert!(result.errors.is_empty());
}

#[test]
fn invalid_context_fixture_fails_validation() {
    let result = guardy_core::validate_config_file(&fixture_path("invalid_context.yaml")).unwrap();
    assert!(!result.valid);
    assert!(result
        .errors
        .iter()
        .any(|error| error.contains("unsupported policy version")));
    assert!(result
        .errors
        .iter()
        .any(|error| error.contains("policy.paths.include")));
    assert!(result
        .errors
        .iter()
        .any(|error| error.contains("forbidden_patterns")));
}

#[test]
fn denied_packet_fixture_is_rejected() {
    let config = guardy_core::ContextConfig::load(&fixture_path("valid_context.yaml")).unwrap();
    let packet = guardy_core::GuardPacket::load(&fixture_path("denied_packet.json")).unwrap();

    let audit = guardy_core::check_packet(&config, &packet);
    assert!(!audit.passed);

    assert!(audit
        .findings
        .iter()
        .any(|finding| finding.rule == "path_exclude"));
    assert!(audit
        .findings
        .iter()
        .any(|finding| finding.rule == "token_cap"));
    assert!(audit
        .findings
        .iter()
        .any(|finding| finding.rule == "runtime_ms_cap"));
    assert!(audit
        .findings
        .iter()
        .any(|finding| finding.rule == "redaction"));
}
