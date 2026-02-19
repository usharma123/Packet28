use std::path::PathBuf;

fn fixture(rel: &str) -> PathBuf {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    workspace.join("tests").join("fixtures").join(rel)
}

#[test]
fn test_ingest_lcov() {
    let path = fixture("lcov/basic.info");
    let data = covy_ingest::ingest_path(&path).unwrap();

    assert_eq!(data.files.len(), 2);
    assert!(data.files.contains_key("src/main.rs"));
    assert!(data.files.contains_key("src/lib.rs"));

    let main = &data.files["src/main.rs"];
    assert_eq!(main.lines_instrumented.len(), 4);
    assert_eq!(main.lines_covered.len(), 3);
}

#[test]
fn test_ingest_cobertura() {
    let path = fixture("cobertura/basic.xml");
    let data = covy_ingest::ingest_path(&path).unwrap();

    assert_eq!(data.files.len(), 1);
    let fc = &data.files["main.py"];
    assert_eq!(fc.lines_instrumented.len(), 5);
    assert_eq!(fc.lines_covered.len(), 3);
}

#[test]
fn test_ingest_jacoco() {
    let path = fixture("jacoco/basic.xml");
    let data = covy_ingest::ingest_path(&path).unwrap();

    assert_eq!(data.files.len(), 2);
    assert!(data.files.contains_key("com/example/App.java"));
    assert!(data.files.contains_key("com/example/Util.java"));

    let app = &data.files["com/example/App.java"];
    assert_eq!(app.lines_instrumented.len(), 4);
    assert_eq!(app.lines_covered.len(), 3);
}

#[test]
fn test_ingest_gocov() {
    let path = fixture("gocov/basic.out");
    let data = covy_ingest::ingest_path(&path).unwrap();

    assert_eq!(data.files.len(), 2);
    assert!(data.files.contains_key("pkg/handler.go"));
    assert!(data.files.contains_key("main.go"));

    let handler = &data.files["pkg/handler.go"];
    assert_eq!(handler.lines_instrumented.len(), 10);
    assert_eq!(handler.lines_covered.len(), 6);
}

#[test]
fn test_format_detection_lcov() {
    use std::path::Path;
    let content = b"TN:test\nSF:src/main.rs\n";
    let format = covy_ingest::detect_format(Path::new("coverage.info"), content).unwrap();
    assert_eq!(format, covy_core::CoverageFormat::Lcov);
}

#[test]
fn test_format_detection_cobertura() {
    use std::path::Path;
    let content = b"<?xml version=\"1.0\" ?>\n<coverage version=\"5\">";
    let format = covy_ingest::detect_format(Path::new("coverage.xml"), content).unwrap();
    assert_eq!(format, covy_core::CoverageFormat::Cobertura);
}

#[test]
fn test_format_detection_jacoco() {
    use std::path::Path;
    let content = b"<?xml version=\"1.0\"?>\n<!DOCTYPE report PUBLIC";
    let format = covy_ingest::detect_format(Path::new("jacoco.xml"), content).unwrap();
    assert_eq!(format, covy_core::CoverageFormat::JaCoCo);
}

#[test]
fn test_format_detection_gocov() {
    use std::path::Path;
    let content = b"mode: set\n";
    let format = covy_ingest::detect_format(Path::new("coverage.out"), content).unwrap();
    assert_eq!(format, covy_core::CoverageFormat::GoCov);
}

#[test]
fn test_merge_coverage_data() {
    let path = fixture("lcov/basic.info");
    let mut data1 = covy_ingest::ingest_path(&path).unwrap();
    let data2 = covy_ingest::ingest_path(&path).unwrap();

    data1.merge(&data2);
    assert_eq!(data1.files.len(), 2);
}

#[test]
fn test_ingest_sarif_diagnostics() {
    let path = fixture("sarif/basic.sarif");
    let data = covy_ingest::ingest_diagnostics_path(&path).unwrap();

    assert_eq!(data.total_issues(), 5);
    assert!(data.issues_by_file.contains_key("src/main.rs"));
    assert!(data.issues_by_file.contains_key("src/lib.rs"));
}

#[test]
fn test_ingest_empty_sarif_diagnostics() {
    let path = fixture("sarif/empty.sarif");
    let data = covy_ingest::ingest_diagnostics_path(&path).unwrap();
    assert_eq!(data.total_issues(), 0);
}

#[test]
fn test_detect_diagnostics_format_sarif() {
    let content = br#"{\"$schema\":\"https://json.schemastore.org/sarif-2.1.0.json\"}"#;
    let format =
        covy_ingest::detect_diagnostics_format(std::path::Path::new("x.sarif"), content).unwrap();
    assert_eq!(format, covy_core::diagnostics::DiagnosticsFormat::Sarif);
}
