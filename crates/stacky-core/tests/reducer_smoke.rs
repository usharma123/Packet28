use std::path::PathBuf;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

#[test]
fn stack_fixture_reduces_and_dedupes() {
    let log = std::fs::read_to_string(fixture_path("basic_stack.log")).unwrap();
    let output = stacky_core::slice(stacky_core::StackSliceRequest {
        log_text: log,
        source: Some("stack-fixture".to_string()),
        max_failures: None,
    });

    assert_eq!(output.total_failures, 3);
    assert_eq!(output.unique_failures, 2);
    assert_eq!(output.duplicates_removed, 1);
    assert_eq!(output.failures[0].occurrences, 2);
    assert!(output.failures[0]
        .first_actionable_frame
        .as_ref()
        .and_then(|frame| frame.file.as_deref())
        .is_some_and(|file| file.contains("src/service.rs")));
}

#[test]
fn stack_packet_json_is_stable() {
    let log = std::fs::read_to_string(fixture_path("basic_stack.log")).unwrap();

    let packet_a = stacky_core::slice_to_packet(stacky_core::StackSliceRequest {
        log_text: log.clone(),
        source: Some("stack-fixture".to_string()),
        max_failures: None,
    });
    let packet_b = stacky_core::slice_to_packet(stacky_core::StackSliceRequest {
        log_text: log,
        source: Some("stack-fixture".to_string()),
        max_failures: None,
    });

    assert_eq!(
        serde_json::to_string(&packet_a).unwrap(),
        serde_json::to_string(&packet_b).unwrap()
    );
    assert_eq!(packet_a.tool.as_deref(), Some("stacky"));
    assert_eq!(packet_a.reducer.as_deref(), Some("slice"));
}
