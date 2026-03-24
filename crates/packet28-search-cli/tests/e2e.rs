use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command as ProcessCommand, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use packet28_daemon_core::{ready_path, socket_path};
use predicates::prelude::*;

fn cli() -> Command {
    Command::new(assert_cmd::cargo::cargo_bin!("packet28-search-cli"))
}

fn daemon_bin() -> std::path::PathBuf {
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest_dir
        .parent()
        .and_then(|path| path.parent())
        .expect("workspace root");
    let status = ProcessCommand::new("cargo")
        .args(["build", "-p", "packet28d"])
        .current_dir(workspace)
        .status()
        .expect("build packet28d");
    assert!(status.success(), "packet28d build failed");
    workspace.join("target/debug/packet28d")
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

fn start_daemon(root: &Path) -> Child {
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut child = ProcessCommand::new(daemon_bin())
        .args(["serve", "--root", canonical_root.to_str().unwrap()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(20) {
        if ready_path(&canonical_root).exists() && socket_path(&canonical_root).exists() {
            return child;
        }
        if let Some(status) = child.try_wait().unwrap() {
            let mut stderr = String::new();
            if let Some(mut stream) = child.stderr.take() {
                let _ = stream.read_to_string(&mut stderr);
            }
            let mut stdout = String::new();
            if let Some(mut stream) = child.stdout.take() {
                let _ = stream.read_to_string(&mut stdout);
            }
            panic!(
                "packet28d exited early for {} with status {status}; stdout={stdout:?} stderr={stderr:?}",
                canonical_root.display()
            );
        }
        thread::sleep(Duration::from_millis(25));
    }
    let _ = child.kill();
    let mut stderr = String::new();
    if let Some(mut stream) = child.stderr.take() {
        let _ = stream.read_to_string(&mut stderr);
    }
    let mut stdout = String::new();
    if let Some(mut stream) = child.stdout.take() {
        let _ = stream.read_to_string(&mut stdout);
    }
    panic!(
        "packet28d did not become ready for {}; stdout={stdout:?} stderr={stderr:?}",
        canonical_root.display()
    );
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
        .stdout(predicate::str::contains("packet28_ms="))
        .stdout(predicate::str::contains("transport=inproc"))
        .stdout(predicate::str::contains("backend=indexed_regex"))
        .stdout(predicate::str::contains("hit=src/lib.rs#L1 pub struct Alpha;"));

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
fn query_command_handles_anchored_line_start_regexes() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src/main.rs"),
        "fn build() {\n    SearchRequest {\n        query: pattern,\n    };\n}\n",
    )
    .unwrap();

    cli()
        .args(["build", dir.path().to_str().unwrap()])
        .assert()
        .success();

    cli()
        .args([
            "query",
            dir.path().to_str().unwrap(),
            r"^\s*SearchRequest\s*\{",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("backend="))
        .stdout(predicate::str::contains("hit=src/main.rs#L2 SearchRequest {"));
}

#[test]
fn bench_command_prints_packet28_and_legacy_timings() {
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
        .stdout(predicate::str::contains("parity=exact"))
        .stdout(predicate::str::contains("packet28_ms="))
        .stdout(predicate::str::contains("legacy_rg_ms="));
}

#[test]
fn query_command_supports_daemon_transport_for_subtree_roots() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path();
    fs::create_dir_all(workspace.join(".git")).unwrap();
    let subtree = workspace.join("crates/search-sample");
    write_fixture(&subtree);

    cli()
        .args(["build", workspace.to_str().unwrap()])
        .assert()
        .success();

    let mut daemon = start_daemon(workspace);

    cli()
        .args([
            "query",
            subtree.to_str().unwrap(),
            "Alpha",
            "--fixed-string",
            "--transport",
            "daemon",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("transport=daemon"))
        .stdout(predicate::str::contains("backend=indexed_regex"))
        .stdout(predicate::str::contains("hit=src/lib.rs#L1 pub struct Alpha;"));

    let _ = daemon.kill();
    let _ = daemon.wait();
}
