use std::fs;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use packet28_state_fs::StateDir;
use serde_json::Value;

use super::{DoctorCheck, McpConfigCheck};
use crate::runtime_integrations::codex;

pub(super) fn mcp_config(root: &Path, home: &Path) -> McpConfigCheck {
    let result = check_mcp(root, home);
    McpConfigCheck {
        path: codex::config_path(home).display().to_string(),
        exists: codex::config_path(home).exists() || root.join(".codex/config.toml").exists(),
        packet28_configured: result.is_ok(),
        detail: result.unwrap_or_else(|error| error.to_string()),
    }
}

pub(super) fn hook_checks(root: &Path) -> Vec<DoctorCheck> {
    let result = check_hooks(root);
    vec![
        DoctorCheck {
            name: "codex_hook_config",
            ok: result.is_ok(),
            required: true,
            detail: result.unwrap_or_else(|error| error.to_string()),
        },
        DoctorCheck {
            name: "codex_hook_trust",
            ok: false,
            required: false,
            detail: "Host hook enablement and trust are not verified. Enable hooks, trust this project, and review generated handlers in Codex /hooks before relying on capture.".to_string(),
        },
    ]
}

fn same_root(root: &Path, configured: &Path) -> bool {
    matches!((root.canonicalize(), configured.canonicalize()), (Ok(expected), Ok(actual)) if expected == actual)
}

fn executable(root: &Path, command: &str) -> bool {
    if command.is_empty() {
        return false;
    }
    if Path::new(command).components().count() == 1 {
        return super::command_resolves(command);
    }
    let Ok(metadata) = fs::metadata(root.join(command)) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    metadata.is_file()
}

// Inspect the two generated config locations without claiming to evaluate
// Codex profiles, managed policy, trust, or a running host's effective config.
fn check_mcp(root: &Path, home: &Path) -> Result<String> {
    let mut server = toml::Table::new();
    let mut sources = Vec::new();
    for path in [codex::config_path(home), root.join(".codex/config.toml")] {
        if !path.exists() {
            continue;
        }
        let config: toml::Value = toml::from_str(&fs::read_to_string(&path)?)
            .with_context(|| format!("invalid Codex TOML in {}", path.display()))?;
        if let Some(entry) = config
            .get("mcp_servers")
            .and_then(|value| value.get("packet28"))
        {
            server.extend(
                entry
                    .as_table()
                    .context("mcp_servers.packet28 must be a table")?
                    .clone(),
            );
            sources.push(path.display().to_string());
        }
    }
    if sources.is_empty() {
        return Err(anyhow!(
            "Packet28 MCP entry missing from Codex user/project config; run setup --runtime codex"
        ));
    }
    if server.get("enabled").and_then(toml::Value::as_bool) == Some(false) {
        return Err(anyhow!("Codex Packet28 MCP entry is disabled"));
    }
    let command = server
        .get("command")
        .and_then(toml::Value::as_str)
        .context("Codex Packet28 MCP entry has no local command")?;
    let cwd = root.join(
        server
            .get("cwd")
            .and_then(toml::Value::as_str)
            .unwrap_or("."),
    );
    if !executable(&cwd, command) {
        return Err(anyhow!("Codex MCP executable is unavailable: {command}"));
    }
    let args = server
        .get("args")
        .and_then(toml::Value::as_array)
        .context("Codex Packet28 MCP entry has no args array")?
        .iter()
        .map(|value| value.as_str().context("Codex MCP args must be strings"))
        .collect::<Result<Vec<_>>>()?;
    let mut roots = Vec::new();
    for (index, arg) in args.iter().enumerate() {
        if *arg == "--root" {
            roots.push(*args.get(index + 1).context("--root requires a path")?);
        } else if let Some(path) = arg.strip_prefix("--root=") {
            roots.push(path);
        }
    }
    if roots.len() != 1 || !same_root(root, &cwd.join(roots[0])) {
        return Err(anyhow!(
            "Codex MCP must specify exactly one --root for {}",
            root.display()
        ));
    }
    Ok(format!("Codex MCP command/workspace valid in {}; profiles, managed policy and host execution are not checked", sources.join(", ")))
}

fn check_hooks(root: &Path) -> Result<String> {
    let directory = StateDir::open(root, &[".codex"], false)?;
    let bytes = directory
        .read_bounded("hooks.json", 1024 * 1024)?
        .context("Codex hooks.json missing; run setup --runtime codex")?;
    let config: Value = serde_json::from_slice(&bytes)?;
    let expected = shell_words::split(
        &crate::cmd_setup::setup_commands::generated_packet28_hook_command("codex", root),
    )?;
    for event in codex::HOOK_EVENTS {
        let entries = config
            .pointer(&format!("/hooks/{event}"))
            .and_then(Value::as_array)
            .with_context(|| format!("Codex hooks.{event} array missing"))?;
        let valid = entries
            .iter()
            .filter(|entry| {
                entry
                    .get("matcher")
                    .is_none_or(|matcher| matches!(matcher.as_str(), Some("" | "*")))
            })
            .filter_map(|entry| entry.get("hooks").and_then(Value::as_array))
            .flatten()
            .any(|hook| {
                if hook.get("type").and_then(Value::as_str) != Some("command") {
                    return false;
                }
                let Some(command) = hook.get("command").and_then(Value::as_str) else {
                    return false;
                };
                if !crate::cmd_setup::setup_commands::is_generated_packet28_hook_command(
                    command, "codex",
                ) {
                    return false;
                }
                let Ok(argv) = shell_words::split(command) else {
                    return false;
                };
                argv.len() == 6
                    && argv[..4] == expected[..4]
                    && executable(root, &argv[4])
                    && same_root(root, &root.join(&argv[5]))
            });
        if !valid {
            return Err(anyhow!("Codex {event} needs an unrestricted Packet28 command handler with a working executable and this workspace root"));
        }
    }
    Ok(format!(
        "Codex lifecycle configuration valid at {}; host execution not tested",
        codex::hooks_path(root).display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn codex_mcp_checks_project_overrides_without_hiding_user_registration() {
        let root = tempdir().unwrap();
        let home = tempdir().unwrap();
        let path = codex::config_path(home.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let binary = std::env::current_exe().unwrap();
        let value: toml::Value = serde_json::from_value(json!({"mcp_servers":{"packet28":{
            "command": binary.to_str().unwrap(), "args":["--root", root.path().to_str().unwrap()]
        }}}))
        .unwrap();
        fs::write(&path, toml::to_string(&value).unwrap()).unwrap();
        assert!(check_mcp(root.path(), home.path()).is_ok());
        let project = root.path().join(".codex/config.toml");
        fs::create_dir_all(project.parent().unwrap()).unwrap();
        fs::write(&project, "model = 'synthetic'\n").unwrap();
        assert!(check_mcp(root.path(), home.path()).is_ok());
        fs::write(&project, "[mcp_servers.packet28]\nenabled = false\n").unwrap();
        assert!(check_mcp(root.path(), home.path())
            .unwrap_err()
            .to_string()
            .contains("disabled"));
        fs::write(
            &project,
            "[mcp_servers.packet28]\nargs = ['--root', '/missing/workspace']\n",
        )
        .unwrap();
        assert!(check_mcp(root.path(), home.path())
            .unwrap_err()
            .to_string()
            .contains("--root"));
    }

    #[test]
    fn codex_hooks_require_every_event_and_do_not_claim_host_trust() {
        let root = tempdir().unwrap();
        let mut config = json!({"hooks":{}});
        let command = crate::cmd_setup::setup_commands::guarded_packet28_hook_command(
            std::env::current_exe().unwrap().to_str().unwrap(),
            "codex",
            root.path(),
        );
        for event in codex::HOOK_EVENTS {
            config["hooks"][event] = json!([{"hooks":[{"type":"command","command":command}]}]);
        }
        let directory = StateDir::open(root.path(), &[".codex"], true).unwrap();
        directory
            .write_atomic("hooks.json", config.to_string().as_bytes())
            .unwrap();
        let checks = hook_checks(root.path());
        assert!(checks[0].ok);
        assert!(!checks[1].ok && !checks[1].required);
        config["hooks"]["PreToolUse"][0]["matcher"] = json!("Read");
        directory
            .write_atomic("hooks.json", config.to_string().as_bytes())
            .unwrap();
        assert!(check_hooks(root.path())
            .unwrap_err()
            .to_string()
            .contains("PreToolUse"));
    }
    #[test]
    fn codex_hook_doctor_rejects_unescaped_shell_expansion_in_literal_paths() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("$PACKET28_LITERAL_WORKSPACE");
        fs::create_dir(&root).unwrap();
        let command = crate::cmd_setup::setup_commands::guarded_packet28_hook_command(
            std::env::current_exe().unwrap().to_str().unwrap(),
            "codex",
            &root,
        );
        let hooks = StateDir::open(&root, &[".codex"], true).unwrap();
        let mut config = json!({"hooks":{}});
        for event in codex::HOOK_EVENTS {
            config["hooks"][event] = json!([{"hooks":[{"type":"command","command":command}]}]);
        }
        hooks
            .write_atomic("hooks.json", config.to_string().as_bytes())
            .unwrap();
        assert!(check_hooks(&root).is_ok());
        let malformed = command.replace("\\$", "$");
        assert_ne!(malformed, command);
        for event in codex::HOOK_EVENTS {
            config["hooks"][event][0]["hooks"][0]["command"] = json!(malformed);
        }
        hooks
            .write_atomic("hooks.json", config.to_string().as_bytes())
            .unwrap();
        assert!(check_hooks(&root).is_err());
    }
}
