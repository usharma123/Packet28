mod support;

use std::fs;
use std::path::Path;
use std::process::Command as ProcessCommand;
use std::time::{Duration, Instant};

use predicates::prelude::*;
use support::{cli, output, stderr_text, stdout_text, write_fixture};

/// Stops the daemon an auto-transport search started for a fixture workspace,
/// including when a failing assertion unwinds. The daemon detaches into its
/// own session, so it would otherwise outlive the temporary workspace.
struct StopWorkspaceDaemon<'a>(&'a Path);

impl Drop for StopWorkspaceDaemon<'_> {
    fn drop(&mut self) {
        use packet28_daemon_protocol::frame::{read_frame, write_frame};
        use packet28_daemon_protocol::message::{DaemonRequest, DaemonResponse};

        let runtime = packet28_daemon_protocol::paths::runtime_path(self.0);
        if !runtime.exists() {
            return;
        }
        if let Ok(mut stream) =
            packet28_daemon_client::transport::connect(self.0, Duration::from_secs(5))
        {
            let _ = write_frame(&mut stream, &DaemonRequest::Stop);
            let _ = read_frame::<_, DaemonResponse>(&mut stream);
        }
        // The daemon removes its runtime metadata as its last cleanup step.
        let started = Instant::now();
        while runtime.exists() && started.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn initialize_git_repository(root: &Path) {
    fs::write(root.join(".gitignore"), ".packet28/\n").unwrap();
    let run = |args: &[&str]| {
        let status = ProcessCommand::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git command failed: {args:?}");
    };
    run(&["init", "--quiet"]);
    run(&["add", "."]);
    run(&[
        "-c",
        "user.name=Packet28 Tests",
        "-c",
        "user.email=packet28-tests@example.invalid",
        "commit",
        "--quiet",
        "-m",
        "fixture",
    ]);
}

#[test]
fn debug_build_prints_generation_and_file_count() {
    let dir = tempfile::tempdir().unwrap();
    write_fixture(dir.path());

    cli()
        .args(["debug", "build", dir.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("build_ms="))
        .stdout(predicate::str::contains("generation="))
        .stdout(predicate::str::contains("files="));
}

#[test]
fn p28_searches_from_repo_root_with_rg_style_output() {
    let dir = tempfile::tempdir().unwrap();
    write_fixture(dir.path());
    let _daemon = StopWorkspaceDaemon(dir.path());

    cli()
        .current_dir(dir.path())
        .args(["Alpha", "--fixed-strings"])
        .assert()
        .success()
        .stdout(predicate::str::contains("src/lib.rs:1:pub struct Alpha;"));
}

#[test]
fn p28_filters_paths_from_current_directory() {
    let dir = tempfile::tempdir().unwrap();
    write_fixture(dir.path());
    let _daemon = StopWorkspaceDaemon(dir.path());

    cli()
        .current_dir(dir.path())
        .args(["handle_value", "src/nested/mod.rs", "--fixed-strings"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "src/nested/mod.rs:2:fn handle_value() { println!(\"beta\"); }",
        ))
        .stdout(predicate::str::contains("src/lib.rs").not());
}

#[test]
fn p28_stats_go_to_stderr_while_hits_stay_on_stdout() {
    let dir = tempfile::tempdir().unwrap();
    write_fixture(dir.path());
    let _daemon = StopWorkspaceDaemon(dir.path());

    cli()
        .args(["debug", "build", dir.path().to_str().unwrap()])
        .assert()
        .success();

    let output = output({
        let mut command = cli();
        command
            .current_dir(dir.path())
            .args(["Alpha", "--fixed-strings", "--stats"]);
        command
    });

    assert!(output.status.success());
    let stdout = stdout_text(&output);
    let stderr = stderr_text(&output);
    assert!(stdout.contains("src/lib.rs:1:pub struct Alpha;"));
    assert!(!stdout.contains("backend="));
    assert!(stderr.contains("p28_ms="));
    assert!(stderr.contains("transport="));
    assert!(stderr.contains("backend="));
}

#[test]
fn inproc_auto_keeps_the_indexed_backend_for_a_clean_git_workspace() {
    let dir = tempfile::tempdir().unwrap();
    write_fixture(dir.path());
    initialize_git_repository(dir.path());
    cli()
        .args(["debug", "build", dir.path().to_str().unwrap()])
        .assert()
        .success();

    cli()
        .current_dir(dir.path())
        .args([
            "alpha_service",
            "--fixed-strings",
            "--transport",
            "inproc",
            "--engine",
            "auto",
            "--stats",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "src/lib.rs:2:pub fn alpha_service() {}",
        ))
        .stderr(predicate::str::contains("backend=indexed_regex"));
}

#[test]
fn inproc_auto_falls_back_for_a_dirty_tracked_non_candidate() {
    let dir = tempfile::tempdir().unwrap();
    write_fixture(dir.path());
    initialize_git_repository(dir.path());
    cli()
        .args(["debug", "build", dir.path().to_str().unwrap()])
        .assert()
        .success();
    fs::write(
        dir.path().join("src/filler_9.rs"),
        "pub fn dirty_non_candidate_workspace_needle() {}\n",
    )
    .unwrap();

    cli()
        .current_dir(dir.path())
        .args([
            "dirty_non_candidate_workspace_needle",
            "--fixed-strings",
            "--transport",
            "inproc",
            "--engine",
            "auto",
            "--stats",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "src/filler_9.rs:1:pub fn dirty_non_candidate_workspace_needle() {}",
        ))
        .stderr(predicate::str::contains("backend=legacy_rg"))
        .stderr(predicate::str::contains(
            "workspace freshness could not be authenticated",
        ));

    cli()
        .current_dir(dir.path())
        .args([
            "dirty_non_candidate_workspace_needle",
            "--fixed-strings",
            "--transport",
            "inproc",
            "--engine",
            "indexed",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("regex search index is not ready"));
}

#[test]
fn inproc_auto_falls_back_for_an_untracked_file() {
    let dir = tempfile::tempdir().unwrap();
    write_fixture(dir.path());
    initialize_git_repository(dir.path());
    cli()
        .args(["debug", "build", dir.path().to_str().unwrap()])
        .assert()
        .success();
    fs::write(
        dir.path().join("src/untracked.rs"),
        "pub fn untracked_workspace_needle() {}\n",
    )
    .unwrap();

    cli()
        .current_dir(dir.path())
        .args([
            "untracked_workspace_needle",
            "--fixed-strings",
            "--transport",
            "inproc",
            "--engine",
            "auto",
            "--stats",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "src/untracked.rs:1:pub fn untracked_workspace_needle() {}",
        ))
        .stderr(predicate::str::contains("backend=legacy_rg"));
}

#[test]
fn inproc_auto_falls_back_after_a_tracked_file_is_renamed() {
    let dir = tempfile::tempdir().unwrap();
    write_fixture(dir.path());
    initialize_git_repository(dir.path());
    cli()
        .args(["debug", "build", dir.path().to_str().unwrap()])
        .assert()
        .success();
    fs::rename(
        dir.path().join("src/lib.rs"),
        dir.path().join("src/renamed.rs"),
    )
    .unwrap();

    cli()
        .current_dir(dir.path())
        .args([
            "alpha_service",
            "--fixed-strings",
            "--transport",
            "inproc",
            "--engine",
            "auto",
            "--stats",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "src/renamed.rs:2:pub fn alpha_service() {}",
        ))
        .stderr(predicate::str::contains("backend=legacy_rg"));
}

#[test]
fn p28_handles_anchored_line_start_regexes() {
    let dir = tempfile::tempdir().unwrap();
    let _daemon = StopWorkspaceDaemon(dir.path());
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src/main.rs"),
        "fn build() {\n    SearchRequest {\n        query: pattern,\n    };\n}\n",
    )
    .unwrap();

    cli()
        .args(["debug", "build", dir.path().to_str().unwrap()])
        .assert()
        .success();

    cli()
        .current_dir(dir.path())
        .args([r"^\s*SearchRequest\s*\{", "--stats"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "src/main.rs:2:    SearchRequest {",
        ))
        .stderr(predicate::str::contains("backend="));
}

#[test]
fn debug_bench_prints_packet28_and_legacy_timings() {
    let dir = tempfile::tempdir().unwrap();
    write_fixture(dir.path());

    cli()
        .args(["debug", "build", dir.path().to_str().unwrap()])
        .assert()
        .success();

    cli()
        .args([
            "debug",
            "bench",
            dir.path().to_str().unwrap(),
            "Alpha",
            "--fixed-strings",
            "--transport",
            "inproc",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("guard=index"))
        .stdout(predicate::str::contains("parity=exact"))
        .stdout(predicate::str::contains("p28_ms="))
        .stdout(predicate::str::contains("legacy_rg_ms="));
}
