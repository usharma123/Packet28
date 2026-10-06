#![cfg(unix)]

#[path = "support/codex_setup.rs"]
mod codex_setup;
#[expect(
    dead_code,
    reason = "this integration binary exercises a focused subset of the shared harness"
)]
#[path = "support/process_harness.rs"]
mod process_harness;

use process_harness::{HarnessLimits, ProcessHarness};
use serde_json::{json, Value};
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct Fixture {
    root: TempDir,
    home: TempDir,
    bin: TempDir,
}

impl Fixture {
    fn new() -> Self {
        process_harness::ensure_packet28d_built();
        let fixture = Self {
            root: TempDir::new().unwrap(),
            home: TempDir::new().unwrap(),
            bin: TempDir::new().unwrap(),
        };
        process_harness::run_git(fixture.root.path(), &["init"]);
        fs::write(
            fixture.root.path().join("README.md"),
            "Codex setup fixture\n",
        )
        .unwrap();
        process_harness::run_git(fixture.root.path(), &["add", "README.md"]);
        process_harness::run_git(
            fixture.root.path(),
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.test",
                "commit",
                "-m",
                "fixture",
            ],
        );
        let codex = fixture.bin.path().join("codex");
        // The registration adapter has a local fallback when this synthetic
        // Codex CLI declines MCP registration. No native host is simulated.
        fs::write(&codex, "#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(&codex, fs::Permissions::from_mode(0o755)).unwrap();
        symlink(
            env!("CARGO_BIN_EXE_Packet28"),
            fixture.bin.path().join("Packet28"),
        )
        .unwrap();
        fixture
    }

    fn configure(&self, command: &mut Command) {
        command.current_dir(self.root.path());
        for (name, relative) in [
            ("HOME", "."),
            ("CODEX_HOME", ".codex"),
            ("CLAUDE_CONFIG_DIR", ".claude"),
            ("XDG_CONFIG_HOME", ".config"),
            ("XDG_DATA_HOME", ".local/share"),
            ("XDG_CACHE_HOME", ".cache"),
            ("XDG_STATE_HOME", ".local/state"),
        ] {
            let path = self.home.path().join(relative);
            fs::create_dir_all(&path).unwrap();
            command.env(name, path);
        }
        command.env(
            "PATH",
            format!("{}:/usr/bin:/bin", self.bin.path().display()),
        );
    }

    fn run(&self, args: &[&str]) -> Vec<u8> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_Packet28"));
        self.configure(&mut command);
        command.args(args);
        let output = ProcessHarness::run(
            &mut command,
            &[],
            Duration::from_secs(30),
            HarnessLimits::default(),
        )
        .unwrap();
        assert!(output.status.success(), "{output:?}");
        output.stdout
    }

    fn setup(&self) {
        self.run(&[
            "setup",
            "--root",
            self.root.path().to_str().unwrap(),
            "--runtime",
            "codex",
            "--yes",
        ]);
        codex_setup::assert_native_hook_installation(self.root.path());
    }

    fn wait_instance_released(&self) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match packet28_daemon_core::task_store_lease::acquire_daemon_instance_lease(
                self.root.path(),
            ) {
                Ok(lease) => {
                    drop(lease);
                    return Ok(());
                }
                Err(packet28_daemon_core::DaemonCoreError::DaemonInstanceAlreadyRunning {
                    ..
                }) => {
                    if Instant::now() >= deadline {
                        return Err("fixture daemon did not release instance authority".to_string());
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(error) => {
                    return Err(format!("fixture daemon authority check failed: {error}"))
                }
            }
        }
    }

    fn runtime_path(&self) -> std::path::PathBuf {
        packet28_daemon_protocol::paths::hook_runtime_config_path(self.root.path())
    }

    fn runtime(&self) -> Value {
        serde_json::from_slice(&fs::read(self.runtime_path()).unwrap()).unwrap()
    }

    fn assert_generated_handler_captures(&self) {
        let hooks: Value =
            serde_json::from_slice(&fs::read(self.root.path().join(".codex/hooks.json")).unwrap())
                .unwrap();
        let generated = hooks["hooks"]["PostToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        let argv = shell_words::split(generated).unwrap();
        assert_eq!(
            fs::canonicalize(&argv[4]).unwrap(),
            Path::new(env!("CARGO_BIN_EXE_Packet28"))
                .canonicalize()
                .unwrap()
        );
        let runtime_bytes = fs::read(self.runtime_path()).unwrap();
        let mut command = Command::new("sh");
        self.configure(&mut command);
        command.args(["-c", generated]);
        let payload = json!({
            "hook_event_name": "PostToolUse",
            "session_id": "reinstalled-codex-session",
            "task_id": "reinstalled-codex-task",
            "cwd": self.root.path(),
            "tool_name": "Bash",
            "tool_input": {"command": "cargo test -p fixture"},
            "tool_response": "test fixture ... ok\n",
        });
        let output = ProcessHarness::run(
            &mut command,
            &serde_json::to_vec(&payload).unwrap(),
            Duration::from_secs(30),
            HarnessLimits::default(),
        )
        .unwrap();
        assert!(output.status.success(), "{output:?}");
        let status: Value = serde_json::from_slice(&self.run(&[
            "daemon",
            "task",
            "status",
            "--root",
            self.root.path().to_str().unwrap(),
            "--task-id",
            "reinstalled-codex-task",
            "--json",
        ]))
        .unwrap();
        assert_eq!(status["task_id"], "reinstalled-codex-task");
        assert_eq!(
            status["latest_hook_session_id"],
            "reinstalled-codex-session"
        );
        assert_eq!(status["latest_hook_command_kind"], "rust_test");
        let events: Value = serde_json::from_slice(&self.run(&["hook", "log", "--json"])).unwrap();
        let event = events
            .as_array()
            .unwrap()
            .iter()
            .find(|event| {
                event["runtime"] == "codex" && event["session_id"] == "reinstalled-codex-session"
            })
            .expect("generated Codex handler must persist its payload");
        assert_eq!(event["event_kind"], "post_tool_use");
        assert_eq!(
            serde_json::from_str::<Value>(event["payload_json"].as_str().unwrap()).unwrap(),
            payload
        );
        assert_eq!(fs::read(self.runtime_path()).unwrap(), runtime_bytes);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let mut command = Command::new(env!("CARGO_BIN_EXE_Packet28"));
        self.configure(&mut command);
        command.args([
            "uninstall",
            "--root",
            self.root.path().to_str().unwrap(),
            "--keep-mcp",
        ]);
        let result = ProcessHarness::run(
            &mut command,
            &[],
            Duration::from_secs(30),
            HarnessLimits::default(),
        );
        let uninstalled = result.as_ref().is_ok_and(|output| output.status.success());
        let released = if uninstalled {
            self.wait_instance_released()
        } else {
            Err("fixture uninstall did not complete".to_string())
        };
        if !uninstalled || released.is_err() {
            eprintln!("fixture cleanup failed: uninstall={result:?}, instance={released:?}");
            assert!(std::thread::panicking(), "fixture uninstall must complete");
        }
    }
}

#[test]
fn codex_uninstall_then_explicit_setup_restores_generated_hook_capture() {
    let fixture = Fixture::new();
    fixture.setup();
    fixture.run(&["uninstall", "--root", fixture.root.path().to_str().unwrap()]);
    assert_eq!(fixture.runtime()["hooks_enabled"], false);
    fixture.wait_instance_released().unwrap();
    fixture.setup();
    assert_eq!(fixture.runtime()["rewrite_enabled"], false);
    fixture.assert_generated_handler_captures();
}

#[test]
fn codex_explicit_setup_reactivates_existing_hooks_without_changing_legacy_preferences() {
    let fixture = Fixture::new();
    fixture.setup();
    let hook_bytes = fs::read(fixture.root.path().join(".codex/hooks.json")).unwrap();
    let mut config = fixture.runtime();
    config["hooks_enabled"] = json!(false);
    config["rewrite_enabled"] = json!(true);
    config["context_budget_tokens"] = json!(1234);
    fs::write(
        fixture.runtime_path(),
        serde_json::to_vec_pretty(&config).unwrap(),
    )
    .unwrap();
    fixture.setup();
    let enabled = fixture.runtime();
    config["hooks_enabled"] = json!(true);
    assert_eq!(enabled, config);
    assert_eq!(
        fs::read(fixture.root.path().join(".codex/hooks.json")).unwrap(),
        hook_bytes
    );
    fixture.assert_generated_handler_captures();
}
