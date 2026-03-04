use std::path::PathBuf;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

#[test]
fn multi_packet_fixture_dedupes_overlapping_refs() {
    let assembled = contextq_core::assemble_packet_files(
        &[fixture_path("packet_a.json"), fixture_path("packet_b.json")],
        contextq_core::AssembleOptions {
            budget_tokens: 1200,
            budget_bytes: 24_000,
            ..contextq_core::AssembleOptions::default()
        },
    )
    .unwrap();

    let payload: contextq_core::AssembledPayload =
        serde_json::from_value(assembled.payload.clone()).unwrap();

    let src_lib_refs = payload
        .refs
        .iter()
        .filter(|reference| reference.kind == "file" && reference.value == "src/lib.rs")
        .count();
    let foo_symbol_refs = payload
        .refs
        .iter()
        .filter(|reference| reference.kind == "symbol" && reference.value == "foo::bar")
        .count();

    assert_eq!(assembled.assembly.input_packets, 2);
    assert_eq!(src_lib_refs, 1);
    assert_eq!(foo_symbol_refs, 1);
}

#[test]
fn budget_fixture_is_trimmed_to_caps() {
    let assembled = contextq_core::assemble_packet_files(
        &[fixture_path("packet_budget.json")],
        contextq_core::AssembleOptions {
            budget_tokens: 40,
            budget_bytes: 400,
            ..contextq_core::AssembleOptions::default()
        },
    )
    .unwrap();

    assert!(assembled.assembly.truncated);
    assert!(assembled.assembly.estimated_tokens <= 40);
    assert!(assembled.assembly.estimated_bytes <= 400);
}
