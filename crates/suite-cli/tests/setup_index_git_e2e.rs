#![cfg(unix)]

mod support;

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use predicates::prelude::*;
use serde_json::{json, Value};
use support::mcp::{
    initialize_mcp_session, packet28_cmd, packet28_process, read_mcp_message_for_id, spawn_mcp,
    stop_mcp_server, write_mcp_message,
};
use support::process_harness::{ensure_packet28d_built, run_git};
use tempfile::TempDir;

const SETUP_MARKER: &str = "Packet28 runtime guidance";
const USER_MARKER: &str = "CommittedUserAlphaToken";
// Each MCP process restarts invocation numbering, and task artifacts are
// immutable, so every search uses a fresh task.
static SEARCHES: AtomicU64 = AtomicU64::new(0);

fn packet28(root: &Path, home: &Path, args: &[&str]) -> assert_cmd::assert::Assert {
    packet28_cmd()
        .current_dir(root)
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .args(args)
        .assert()
}

fn root_arg(root: &Path) -> &str {
    root.to_str().unwrap()
}

fn index_status(root: &Path, home: &Path) -> Value {
    let output = packet28(
        root,
        home,
        &[
            "daemon",
            "index",
            "status",
            "--root",
            root_arg(root),
            "--json",
        ],
    )
    .success()
    .get_output()
    .stdout
    .clone();
    serde_json::from_slice(&output).unwrap()
}

fn wait_for_settled_status(root: &Path, home: &Path) -> Value {
    let started = Instant::now();
    loop {
        let status = index_status(root, home);
        let regex = status["manifest"]["regex_status"].as_str().unwrap_or("");
        if status["ready"].as_bool() == Some(true) || matches!(regex, "stale" | "corrupt") {
            return status;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "index status did not settle: {status}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Starts the daemon again to reload persisted index state.
///
/// `daemon stop` can currently return before the old process releases its
/// instance lock (tracked separately from this index contract), so a start that
/// loses that race is retried within a bounded deadline.
fn start_stopped_daemon(root: &Path, home: &Path) {
    let started = Instant::now();
    loop {
        let output = packet28_cmd()
            .current_dir(root)
            .env("HOME", home)
            .env("PATH", "/usr/bin:/bin")
            .args(["daemon", "start", "--root", root_arg(root)])
            .output()
            .unwrap();
        if output.status.success() {
            return;
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("did not become ready") && started.elapsed() < Duration::from_secs(30),
            "daemon restart failed: {stderr}"
        );
    }
}

fn regex_manifest(root: &Path) -> Value {
    let raw = fs::read(root.join(".packet28/index/regex-v1/manifest.json")).unwrap();
    serde_json::from_slice(&raw).unwrap()
}

fn head_commit(root: &Path) -> String {
    let head = fs::read_to_string(root.join(".git/HEAD")).unwrap();
    let reference = head.trim().strip_prefix("ref: ").unwrap();
    fs::read_to_string(root.join(".git").join(reference))
        .unwrap()
        .trim()
        .to_string()
}

fn indexed_search(root: &Path, query: &str) -> Value {
    let mut command = packet28_process();
    command
        .current_dir(root)
        .args(["mcp", "serve", "--root", root_arg(root)]);
    let mut server = spawn_mcp(&mut command);
    initialize_mcp_session(&mut server);
    write_mcp_message(
        &mut server,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "packet28.search",
                "arguments": {
                    "task_id": format!("task-setup-git-index-{}", SEARCHES.fetch_add(1, Ordering::Relaxed)),
                    "query": query,
                    "fixed_string": true,
                    "response_mode": "full"
                }
            }
        }),
    );
    let search = read_mcp_message_for_id(&mut server, 2);
    stop_mcp_server(server);
    let result = search["result"]["structuredContent"].clone();
    if result.is_null() {
        let log =
            fs::read_to_string(root.join(".packet28/daemon/packet28d.log")).unwrap_or_default();
        let tasks = walk(&root.join(".packet28/task"));
        panic!("{query}: {search}\ntask tree: {tasks:?}\ndaemon log:\n{log}");
    }
    result
}

fn walk(path: &Path) -> Vec<String> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(path).into_iter().flatten().flatten() {
        let file_type = entry.file_type().unwrap();
        entries.push(format!("{} {file_type:?}", entry.path().display()));
        if file_type.is_dir() {
            entries.extend(walk(&entry.path()));
        }
    }
    entries
}

fn assert_indexed_match(root: &Path, query: &str, path: &str) {
    let result = indexed_search(root, query);
    assert_eq!(
        result["engine"]["engine"].as_str(),
        Some("indexed_regex"),
        "{query}: {result}"
    );
    assert!(
        result["match_count"].as_u64().unwrap_or(0) >= 1,
        "{query}: {result}"
    );
    assert!(
        result.to_string().contains(path),
        "{query} did not resolve {path}: {result}"
    );
}

#[test]
fn setup_in_a_clean_git_repository_publishes_an_attested_index_over_its_own_changes() {
    ensure_packet28d_built();
    let root = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let root = root.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        format!("pub struct {USER_MARKER};\n"),
    )
    .unwrap();
    run_git(root, &["init", "--quiet"]);
    run_git(root, &["add", "."]);
    run_git(
        root,
        &[
            "-c",
            "user.name=Packet28 Test",
            "-c",
            "user.email=packet28@example.invalid",
            "commit",
            "--quiet",
            "--no-gpg-sign",
            "-m",
            "fixture",
        ],
    );
    let head = head_commit(root);

    packet28(
        root,
        home.path(),
        &[
            "setup",
            "--root",
            root_arg(root),
            "--runtime",
            "cursor",
            "--yes",
        ],
    )
    .success()
    .stdout(predicate::str::contains("index ready"))
    .stdout(predicate::str::contains("index deferred").not());

    let setup_file = root.join(".cursor/rules/packet28.mdc");
    assert!(fs::read_to_string(&setup_file)
        .unwrap()
        .contains(SETUP_MARKER));
    assert!(root.join(".gitignore").is_file());
    let manifest = regex_manifest(root);
    assert_eq!(manifest["base_commit"].as_str(), Some(head.as_str()));
    assert_eq!(
        manifest["workspace_attested_commit"].as_str(),
        Some(head.as_str()),
        "{manifest}"
    );
    assert!(
        manifest.get("workspace_clean_commit").is_none(),
        "setup-dirtied build was labeled clean: {manifest}"
    );
    let generation = manifest["generation"].as_u64().unwrap();

    assert_indexed_match(root, SETUP_MARKER, ".cursor/rules/packet28.mdc");
    assert_indexed_match(root, USER_MARKER, "src/lib.rs");

    packet28(
        root,
        home.path(),
        &["daemon", "stop", "--root", root_arg(root)],
    )
    .success();
    start_stopped_daemon(root, home.path());
    let reloaded = wait_for_settled_status(root, home.path());
    assert_eq!(reloaded["ready"].as_bool(), Some(true), "{reloaded}");
    assert_eq!(
        reloaded["manifest"]["regex_generation"].as_u64(),
        Some(generation),
        "reload rebuilt instead of verifying the attested generation: {reloaded}"
    );
    assert_indexed_match(root, SETUP_MARKER, ".cursor/rules/packet28.mdc");

    packet28(
        root,
        home.path(),
        &["daemon", "stop", "--root", root_arg(root)],
    )
    .success();
    fs::write(
        root.join("src/lib.rs"),
        "pub struct UnreportedReplacement;\n",
    )
    .unwrap();
    start_stopped_daemon(root, home.path());
    let after_edit = wait_for_settled_status(root, home.path());
    let served_old_generation = after_edit["ready"].as_bool() == Some(true)
        && after_edit["manifest"]["regex_generation"].as_u64() == Some(generation);
    assert!(
        !served_old_generation,
        "an unreported edit reloaded the attested generation as fresh: {after_edit}"
    );
    let stale_search = indexed_search(root, USER_MARKER);
    assert!(
        stale_search["engine"]["engine"].as_str() != Some("indexed_regex")
            || stale_search["match_count"].as_u64() == Some(0),
        "stale indexed bytes were served after an unreported edit: {stale_search}"
    );

    packet28(
        root,
        home.path(),
        &["daemon", "stop", "--root", root_arg(root)],
    )
    .success();
}
