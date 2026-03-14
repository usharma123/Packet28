use crate::{slice, slice_to_packet, StackSliceRequest};

#[test]
fn parses_and_dedupes_repeated_failures() {
    let input = r#"
java.lang.IllegalStateException: boom
  at com.example.Service.run(Service.java:42)
  at com.example.Main.main(Main.java:10)

java.lang.IllegalStateException: boom
  at com.example.Service.run(Service.java:42)
  at com.example.Main.main(Main.java:10)

thread 'main' panicked at src/lib.rs:11:2
  0: app::core::run at src/lib.rs:11:2
  1: std::rt::lang_start_internal at /rustc/abc/std/src/rt.rs:95:18
"#;

    let output = slice(StackSliceRequest {
        log_text: input.to_string(),
        source: Some("fixture".to_string()),
        max_failures: None,
    });

    assert_eq!(output.total_failures, 3);
    assert_eq!(output.unique_failures, 2);
    assert_eq!(output.duplicates_removed, 1);
    assert_eq!(output.failures[0].occurrences, 2);
}

#[test]
fn packet_output_is_deterministic() {
    let input = r#"
panic: failed to connect
  at service::dial src/net.rs:40:3
"#;

    let packet_a = slice_to_packet(StackSliceRequest {
        log_text: input.to_string(),
        source: Some("a".to_string()),
        max_failures: None,
    });
    let packet_b = slice_to_packet(StackSliceRequest {
        log_text: input.to_string(),
        source: Some("a".to_string()),
        max_failures: None,
    });

    assert_eq!(
        serde_json::to_string(&packet_a).unwrap(),
        serde_json::to_string(&packet_b).unwrap()
    );
    assert_eq!(packet_a.packet_id.as_deref(), Some("stacky-slice-v1"));
}
