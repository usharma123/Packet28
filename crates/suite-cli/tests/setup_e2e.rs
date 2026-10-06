#[cfg(unix)]
#[path = "support/codex_setup.rs"]
mod codex_setup;

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
#[cfg(unix)]
use std::sync::{Mutex, MutexGuard, OnceLock};
use tempfile::TempDir;

fn suite_cmd() -> Command {
    assert_cmd::cargo::cargo_bin_cmd!("Packet28")
}

#[cfg(unix)]
fn setup_e2e_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
}

#[cfg(unix)]
fn write_fake_codex_binary(path: &Path, log_path: &Path) {
    let script = format!(
        "#!/bin/sh\n\
set -eu\n\
printf '%s\\n' \"$*\" >> \"{log_path}\"\n\
if [ \"${{1:-}}\" = \"mcp\" ] && [ \"${{2:-}}\" = \"add\" ] && [ \"${{3:-}}\" = \"packet28\" ]; then\n\
  shift 3\n\
  if [ \"${{1:-}}\" = \"--\" ]; then\n\
    shift\n\
  fi\n\
  command_name=\"${{1:-packet28-mcp}}\"\n\
  shift || true\n\
  root=\"\"\n\
  while [ \"$#\" -gt 0 ]; do\n\
    if [ \"$1\" = \"--root\" ]; then\n\
      root=\"$2\"\n\
      break\n\
    fi\n\
    shift\n\
  done\n\
  mkdir -p \"$HOME/.codex\"\n\
  cat > \"$HOME/.codex/config.toml\" <<EOF\n\
[mcp_servers.packet28]\n\
command = \"$command_name\"\n\
args = [\"--root\", \"$root\"]\n\
EOF\n\
  exit 0\n\
fi\n\
exit 0\n",
        log_path = log_path.display(),
    );
    fs::write(path, script).unwrap();
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).unwrap();
}

#[test]
#[cfg(unix)]
fn test_setup_only_writes_artifacts_for_detected_runtimes() {
    let root = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    let codex_log = home.path().join("codex-cli.log");
    write_fake_codex_binary(&bin_dir.path().join("codex"), &codex_log);

    suite_cmd()
        .current_dir(root.path())
        .env("HOME", home.path())
        .env("CODEX_HOME", home.path().join(".codex"))
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", bin_dir.path().display()),
        )
        .args(["setup", "--root", root.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Your agent runtimes are configured to use Packet28 control-plane MCP tools.",
        ));

    assert!(root.path().join("AGENTS.md").exists());
    codex_setup::assert_native_hook_installation(root.path());
    assert!(!root.path().join("CLAUDE.md").exists());
    assert!(!root.path().join(".cursorrules").exists());
    assert!(home.path().join(".codex").join("config.toml").exists());
    assert!(fs::read_to_string(codex_log)
        .unwrap()
        .contains("mcp add packet28 -- packet28-mcp --root"));
}

#[test]
#[cfg(unix)]
fn test_setup_codex_writes_mcp_agents_and_native_hooks() {
    let root = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    let codex_log = home.path().join("codex-cli.log");
    write_fake_codex_binary(&bin_dir.path().join("codex"), &codex_log);

    suite_cmd()
        .current_dir(root.path())
        .env("HOME", home.path())
        .env("CODEX_HOME", home.path().join(".codex"))
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", bin_dir.path().display()),
        )
        .args([
            "setup",
            "--root",
            root.path().to_str().unwrap(),
            "--runtime",
            "codex",
            "--yes",
        ])
        .assert()
        .success();

    assert!(root.path().join("AGENTS.md").exists());
    codex_setup::assert_native_hook_installation(root.path());
    let config = fs::read_to_string(home.path().join(".codex").join("config.toml")).unwrap();
    assert!(config.contains("[mcp_servers.packet28]"));
    assert!(!config.contains("codex_hooks = true"));
    assert!(fs::read_to_string(codex_log)
        .unwrap()
        .contains("mcp add packet28 -- packet28-mcp --root"));
}

#[test]
#[cfg(unix)]
fn test_setup_cline_writes_instruction_only_rules_without_mcp_or_hooks() {
    let _guard = setup_e2e_lock();
    let root = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();

    suite_cmd()
        .current_dir(root.path())
        .env("HOME", home.path())
        .env("PATH", "/usr/bin:/bin")
        .args([
            "setup",
            "--root",
            root.path().to_str().unwrap(),
            "--runtime",
            "cline",
            "--yes",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("instruction files only"));

    let rules_path = root.path().join(".clinerules");
    assert!(rules_path.exists());
    assert!(fs::read_to_string(rules_path)
        .unwrap()
        .contains("Packet28 Guidance"));
    assert!(!root.path().join(".mcp.json").exists());
    assert!(!root.path().join(".claude").join("settings.json").exists());
    assert!(!root.path().join(".cursor").join("hooks.json").exists());

    suite_cmd()
        .current_dir(root.path())
        .env("HOME", home.path())
        .env("PATH", "/usr/bin:/bin")
        .args([
            "doctor",
            "--agent",
            "cline",
            "--root",
            root.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("instruction_file"))
        .stdout(predicate::str::contains("guidance-only"));
}
