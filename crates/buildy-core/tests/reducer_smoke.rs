use std::path::PathBuf;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

#[test]
fn build_fixture_reduces_and_groups() {
    let log = std::fs::read_to_string(fixture_path("basic_build.log")).unwrap();
    let output = buildy_core::reduce(buildy_core::BuildReduceRequest {
        log_text: log,
        source: Some("build-fixture".to_string()),
        max_diagnostics: None,
    });

    assert_eq!(output.total_diagnostics, 4);
    assert_eq!(output.unique_diagnostics, 3);
    assert_eq!(output.duplicates_removed, 1);
    assert!(output
        .groups
        .iter()
        .any(|group| group.root_cause.contains("E0425")));
    assert!(output
        .ordered_fixes
        .first()
        .is_some_and(|entry| entry.contains("error")));
}

#[test]
fn build_packet_json_is_stable() {
    let log = std::fs::read_to_string(fixture_path("basic_build.log")).unwrap();

    let packet_a = buildy_core::reduce_to_packet(buildy_core::BuildReduceRequest {
        log_text: log.clone(),
        source: Some("build-fixture".to_string()),
        max_diagnostics: None,
    });
    let packet_b = buildy_core::reduce_to_packet(buildy_core::BuildReduceRequest {
        log_text: log,
        source: Some("build-fixture".to_string()),
        max_diagnostics: None,
    });

    assert_eq!(
        serde_json::to_string(&packet_a).unwrap(),
        serde_json::to_string(&packet_b).unwrap()
    );
    assert_eq!(packet_a.tool.as_deref(), Some("buildy"));
    assert_eq!(packet_a.reducer.as_deref(), Some("reduce"));
}
