use std::fs;
use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;

fn cli() -> Command {
    Command::new(assert_cmd::cargo::cargo_bin!("packet28-search-cli"))
}

fn write_fixture(root: &Path) {
    fs::create_dir_all(root.join("src/nested")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub struct Alpha;\npub fn alpha_service() {}\nconst ALPHA: &str = \"Alpha\";\n",
    )
    .unwrap();
    fs::write(
        root.join("src/nested/mod.rs"),
        "pub enum Beta { AlphaVariant }\nfn handle_value() { println!(\"beta\"); }\n",
    )
    .unwrap();
    for idx in 0..10 {
        fs::write(
            root.join("src").join(format!("filler_{idx}.rs")),
            format!("pub fn filler_{idx}() {{ println!(\"beta_{idx}\"); }}\n"),
        )
        .unwrap();
    }
}

#[test]
fn build_command_prints_generation_and_file_count() {
    let dir = tempfile::tempdir().unwrap();
    write_fixture(dir.path());

    let mut command = cli();
    command
        .args(["build", dir.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("build_ms="))
        .stdout(predicate::str::contains("generation="))
        .stdout(predicate::str::contains("files="));
}

#[test]
fn query_and_guard_commands_report_expected_results() {
    let dir = tempfile::tempdir().unwrap();
    write_fixture(dir.path());

    cli()
        .args(["build", dir.path().to_str().unwrap()])
        .assert()
        .success();

    cli()
        .args([
            "query",
            dir.path().to_str().unwrap(),
            "Alpha",
            "--fixed-string",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("indexed_ms="))
        .stdout(predicate::str::contains(
            "sample=src/lib.rs#L1 pub struct Alpha;",
        ));

    cli()
        .args([
            "guard",
            dir.path().to_str().unwrap(),
            "Alpha",
            "--fixed-string",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("mode=index"));

    cli()
        .args(["guard", dir.path().to_str().unwrap(), ".+"])
        .assert()
        .success()
        .stdout(predicate::str::contains("mode=fallback"));
}

#[test]
fn bench_command_prints_indexed_and_legacy_timings() {
    let dir = tempfile::tempdir().unwrap();
    write_fixture(dir.path());

    cli()
        .args(["build", dir.path().to_str().unwrap()])
        .assert()
        .success();

    cli()
        .args([
            "bench",
            dir.path().to_str().unwrap(),
            "Alpha",
            "--fixed-string",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("guard=index"))
        .stdout(predicate::str::contains("indexed_ms="))
        .stdout(predicate::str::contains("legacy_rg_ms="));
}
