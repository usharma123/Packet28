use crate::{reduce, reduce_to_packet, BuildReduceRequest};

#[test]
fn parses_and_groups_diagnostics() {
    let input = r#"
src/lib.rs:10:5: error: cannot find value `x` in this scope [E0425]
src/lib.rs:10:5: error: cannot find value `x` in this scope [E0425]
main.c(40,2): warning C4996: use of deprecated function
error[E0308]: mismatched types
  --> src/main.rs:22:13
"#;

    let output = reduce(BuildReduceRequest {
        log_text: input.to_string(),
        source: Some("fixture".to_string()),
        max_diagnostics: None,
    });

    assert_eq!(output.total_diagnostics, 4);
    assert_eq!(output.unique_diagnostics, 3);
    assert_eq!(output.duplicates_removed, 1);
    assert!(!output.groups.is_empty());
    assert!(output
        .ordered_fixes
        .first()
        .is_some_and(|entry| entry.contains("error")));
}

#[test]
fn packet_output_is_deterministic() {
    let input = "src/lib.rs:1:1: warning: unused import [W100]";

    let packet_a = reduce_to_packet(BuildReduceRequest {
        log_text: input.to_string(),
        source: Some("a".to_string()),
        max_diagnostics: None,
    });
    let packet_b = reduce_to_packet(BuildReduceRequest {
        log_text: input.to_string(),
        source: Some("a".to_string()),
        max_diagnostics: None,
    });

    assert_eq!(
        serde_json::to_string(&packet_a).unwrap(),
        serde_json::to_string(&packet_b).unwrap()
    );
    assert_eq!(packet_a.packet_id.as_deref(), Some("buildy-reduce-v1"));
}
