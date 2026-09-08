use super::*;
use packet28_daemon_protocol::hooks::HookRuntimeConfig;
use packet28_daemon_protocol::index::{DaemonIndexManifest, DaemonIndexStatusResponse};
use tempfile::tempdir;

fn runtime(name: &'static str, slug: &'static str, detected: bool, has_mcp: bool) -> RuntimeInfo {
    let adapter = crate::runtime_integrations::adapter_for_slug(slug)
        .unwrap_or_else(|| panic!("unknown runtime slug: {slug}"));
    assert_eq!(adapter.name, name);
    RuntimeInfo {
        adapter,
        name,
        slug,
        prompt_targets: has_mcp
            .then(|| PromptTarget {
                path: PathBuf::from(format!("{slug}.md")),
                format: agent_surface::AgentPromptFormat::Agents,
            })
            .into_iter()
            .collect(),
        detected,
    }
}

#[test]
fn select_setup_runtimes_prefers_detected_runtimes_for_all() {
    let runtimes = vec![
        runtime("Claude Code", "claude", false, true),
        runtime("Cursor", "cursor", false, true),
        runtime("Codex", "codex", true, false),
        runtime("Windsurf", "windsurf", true, false),
    ];
    let choice = SetupPlanChoice {
        mode: SetupMode::Recommended,
        runtime_scope: SetupRuntimeScope::Detected,
        fallback_only: false,
    };

    let selected = select_setup_runtimes(&runtimes, &choice);
    let slugs: Vec<&str> = selected.iter().map(|runtime| runtime.slug).collect();

    assert_eq!(slugs, vec!["codex", "windsurf"]);
}

#[test]
fn select_setup_runtimes_supports_all_and_single_scopes() {
    let runtimes = vec![
        runtime("Claude Code", "claude", false, true),
        runtime("Cursor", "cursor", true, true),
    ];
    let all_choice = SetupPlanChoice {
        mode: SetupMode::Custom,
        runtime_scope: SetupRuntimeScope::All,
        fallback_only: false,
    };
    let single_choice = SetupPlanChoice {
        mode: SetupMode::Custom,
        runtime_scope: SetupRuntimeScope::Single("claude".to_string()),
        fallback_only: false,
    };

    let all_selected = select_setup_runtimes(&runtimes, &all_choice);
    let all_slugs: Vec<&str> = all_selected.iter().map(|runtime| runtime.slug).collect();
    let single_selected = select_setup_runtimes(&runtimes, &single_choice);
    let single_slugs: Vec<&str> = single_selected.iter().map(|runtime| runtime.slug).collect();

    assert_eq!(all_slugs, vec!["claude", "cursor"]);
    assert_eq!(single_slugs, vec!["claude"]);
}

#[test]
fn explicit_setup_choice_maps_default_flags_to_recommended() {
    let runtimes = vec![
        runtime("Claude Code", "claude", true, true),
        runtime("Codex", "codex", true, true),
    ];
    let args = SetupArgs {
        root: ".".to_string(),
        yes: true,
        fallback_only: false,
        runtime: "all".to_string(),
    };

    let choice = explicit_setup_choice(&args, &runtimes).unwrap();

    assert_eq!(
        choice,
        SetupPlanChoice {
            mode: SetupMode::Recommended,
            runtime_scope: SetupRuntimeScope::Detected,
            fallback_only: false,
        }
    );
}

#[test]
fn explicit_setup_choice_maps_runtime_override_to_custom_single_scope() {
    let runtimes = vec![runtime("Claude Code", "claude", false, true)];
    let args = SetupArgs {
        root: ".".to_string(),
        yes: false,
        fallback_only: false,
        runtime: "claude".to_string(),
    };

    let choice = explicit_setup_choice(&args, &runtimes).unwrap();

    assert_eq!(
        choice,
        SetupPlanChoice {
            mode: SetupMode::Custom,
            runtime_scope: SetupRuntimeScope::Single("claude".to_string()),
            fallback_only: false,
        }
    );
}

#[test]
fn detect_runtimes_includes_instruction_only_parity_targets() {
    let root = tempdir().unwrap();
    let command_exists = |_name: &str| false;
    let run_command = |_name: &str, _args: &[String]| Ok(false);
    let environment =
        RuntimeEnvironment::new(root.path(), root.path(), &command_exists, &run_command);
    let runtimes = detect_runtimes(&environment);
    let by_slug = runtimes
        .iter()
        .map(|runtime| (runtime.slug, runtime))
        .collect::<std::collections::BTreeMap<_, _>>();

    assert_eq!(
        by_slug["copilot"].prompt_targets[0].path,
        root.path().join(".github").join("copilot-instructions.md")
    );
    assert_eq!(
        by_slug["gemini"].prompt_targets[0].path,
        root.path().join("GEMINI.md")
    );
    assert_eq!(
        by_slug["cline"].prompt_targets[0].path,
        root.path().join(".clinerules")
    );
    assert_eq!(
        by_slug["roo"].prompt_targets[0].path,
        root.path().join(".roo").join("rules").join("packet28.md")
    );
    assert_eq!(
        by_slug["kilocode"].prompt_targets[0].path,
        root.path()
            .join(".kilocode")
            .join("rules")
            .join("packet28-rules.md")
    );
    assert_eq!(
        by_slug["antigravity"].prompt_targets[0].path,
        root.path()
            .join(".agents")
            .join("rules")
            .join("antigravity-packet28-rules.md")
    );

    assert!(by_slug["copilot"].adapter.mcp.is_none());
    assert!(by_slug["copilot"].adapter.hooks.is_some());
    assert!(by_slug["gemini"].adapter.mcp.is_none());
    assert!(by_slug["gemini"].adapter.hooks.is_some());
    assert!(by_slug["opencode"].adapter.mcp.is_none());
    assert!(by_slug["opencode"].adapter.hooks.is_some());
    assert!(by_slug["hermes"].adapter.mcp.is_none());
    assert!(by_slug["hermes"].adapter.hooks.is_some());

    for slug in ["cline", "roo", "kilocode", "antigravity"] {
        assert!(by_slug[slug].adapter.mcp.is_none());
        assert!(by_slug[slug].adapter.hooks.is_none());
    }
}

#[test]
fn write_claude_hook_config_installs_packet28_hooks() {
    let dir = tempdir().unwrap();
    let path = dir.path().join(".claude").join("settings.json");
    let status = setup_hooks::write_claude_hook_config(&path, dir.path(), true).unwrap();
    assert!(matches!(status, McpConfigStatus::Written));
    let value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    // Hooks should be at top-level event keys, not nested under "packet28".
    assert!(value["hooks"]["SessionStart"].is_array());
    assert!(value["hooks"]["PostToolUse"].is_array());
    assert!(value["hooks"]["PostToolUseFailure"].is_array());
    assert!(value["hooks"].get("packet28").is_none());
    assert_eq!(
        value["hooks"]["SessionStart"][0]["hooks"][0]["type"].as_str(),
        Some("command")
    );
    let session_start_command = value["hooks"]["SessionStart"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert!(session_start_command.contains("${CLAUDE_PROJECT_DIR}"));
    assert!(!session_start_command.contains(dir.path().to_str().unwrap()));
    assert_eq!(
        value["hooks"]["SessionStart"][0]["matcher"].as_str(),
        Some("startup|resume|clear|compact")
    );
    assert_eq!(
        value["hooks"]["SessionStart"][1]["matcher"].as_str(),
        Some("fork")
    );
    assert_eq!(
        value["hooks"]["SessionStart"][1]["hooks"][0]["type"].as_str(),
        Some("http")
    );
    assert_eq!(
        value["hooks"]["UserPromptSubmit"][0]["hooks"][0]["type"].as_str(),
        Some("command")
    );
    assert!(value["hooks"]["UserPromptSubmit"][0]
        .get("matcher")
        .is_none());
    assert_eq!(
        value["hooks"]["PreToolUse"][0]["hooks"][0]["type"].as_str(),
        Some("http")
    );
    assert_eq!(
        value["hooks"]["PreToolUse"][0]["matcher"].as_str(),
        Some("*")
    );
    assert_eq!(
        value["hooks"]["Stop"][0]["hooks"][0]["type"].as_str(),
        Some("http")
    );
    assert!(value["hooks"]["Stop"][0].get("matcher").is_none());
    let http_url = value["hooks"]["PreToolUse"][0]["hooks"][0]["url"]
        .as_str()
        .unwrap();
    assert!(http_url.starts_with("http://127.0.0.1:"));
    assert_eq!(
        value["hooks"]["SessionStart"][1]["hooks"][0]["url"].as_str(),
        Some(http_url)
    );
    assert_eq!(
        value["allowedHttpHookUrls"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>(),
        vec![http_url]
    );
}

#[test]
fn project_mcp_config_uses_relocation_safe_root() {
    let dir = tempdir().unwrap();
    let path = dir.path().join(".mcp.json");

    let status = write_mcp_config(&path, dir.path(), true).unwrap();

    assert!(matches!(status, McpConfigStatus::Written));
    let value: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(
        value["mcpServers"]["packet28"]["args"],
        json!(["--root", ".", "--toolset", "core"])
    );
}

#[test]
fn generated_packet28_hook_command_exits_zero_when_binary_is_missing() {
    let dir = tempdir().unwrap();
    for runtime in ["claude", "cursor", "copilot", "gemini", "windsurf"] {
        let command = guarded_packet28_hook_command("/missing/Packet28", runtime, dir.path());
        assert!(command.contains(&format!(" hook {runtime} ")));
        assert!(command.contains("exit 0"));

        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(&command)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "generated {runtime} hook failed: status={:?} stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn write_claude_hook_config_replaces_legacy_command_hooks() {
    let dir = tempdir().unwrap();
    let path = dir.path().join(".claude").join("settings.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let command = resolve_packet28_cli_command();
    let root_arg = shell_escape(dir.path().display().to_string());
    let hook_command = format!("{command} hook claude --root \"{root_arg}\"");
    fs::write(
        &path,
        serde_json::to_string_pretty(&json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": ".*",
                    "hooks": [{"type": "command", "command": hook_command}]
                }]
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let status = setup_hooks::write_claude_hook_config(&path, dir.path(), true).unwrap();
    assert!(matches!(status, McpConfigStatus::Written));
    let value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let entries = value["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["hooks"][0]["type"].as_str(), Some("http"));
}

#[test]
fn write_claude_hook_config_removes_stale_packet28_command_paths() {
    let dir = tempdir().unwrap();
    let path = dir.path().join(".claude").join("settings.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "SessionStart": [
                        {
                            "matcher": "startup|resume|clear|compact",
                            "hooks": [{"type": "command", "command": "/missing/Packet28 hook claude --root \"/tmp/demo\""}]
                        },
                        {
                            "matcher": "startup|resume|clear|compact",
                            "hooks": [{"type": "command", "command": "/other/tool"}]
                        }
                    ]
                }
            }))
            .unwrap(),
        )
        .unwrap();

    let status = setup_hooks::write_claude_hook_config(&path, dir.path(), true).unwrap();
    assert!(matches!(status, McpConfigStatus::Written));

    let value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let entries = value["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(entries.len(), 3);
    assert!(entries.iter().any(|entry| {
        entry["matcher"].as_str() == Some("fork")
            && entry["hooks"][0]["type"].as_str() == Some("http")
    }));
    let commands = entries
        .iter()
        .filter_map(|entry| entry["hooks"][0]["command"].as_str())
        .collect::<Vec<_>>();
    assert!(commands
        .iter()
        .any(|command| command.contains(" hook claude ")));
    assert!(commands.contains(&"/other/tool"));
    assert!(!commands
        .iter()
        .any(|command| command.starts_with("/missing/Packet28")));
}

#[test]
fn write_cursor_hook_config_installs_packet28_hooks() {
    let dir = tempdir().unwrap();
    let path = dir.path().join(".cursor").join("hooks.json");
    let status = setup_hooks::write_cursor_hook_config(&path, dir.path(), true).unwrap();
    assert!(matches!(status, McpConfigStatus::Written));
    let value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert!(value["hooks"]["beforeSubmitPrompt"].is_array());
    assert!(value["hooks"]["beforeShellExecution"].is_array());
    assert!(value["hooks"]["afterShellExecution"].is_array());
    assert!(value["hooks"]["stop"].is_array());
}

#[test]
fn write_gemini_hook_config_installs_packet28_before_tool_hook() {
    let dir = tempdir().unwrap();
    let path = dir.path().join(".gemini").join("settings.json");
    let status = setup_hooks::write_gemini_hook_config(&path, dir.path(), true).unwrap();
    assert!(matches!(status, McpConfigStatus::Written));
    let value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let hooks = value["hooks"]["BeforeTool"].as_array().unwrap();
    assert_eq!(hooks.len(), 1);
    assert_eq!(hooks[0]["matcher"].as_str(), Some("run_shell_command"));
    let command = hooks[0]["hooks"][0]["command"].as_str().unwrap();
    assert!(command.contains(" hook gemini "));
}

#[test]
fn write_copilot_hook_config_installs_packet28_pretool_hook() {
    let dir = tempdir().unwrap();
    let path = dir
        .path()
        .join(".github")
        .join("hooks")
        .join("packet28-rewrite.json");
    let status = setup_hooks::write_copilot_hook_config(&path, dir.path(), true).unwrap();
    assert!(matches!(status, McpConfigStatus::Written));
    let value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let hooks = value["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(hooks.len(), 1);
    assert_eq!(hooks[0]["type"].as_str(), Some("command"));
    assert_eq!(hooks[0]["timeout"].as_i64(), Some(5));
    let command = hooks[0]["command"].as_str().unwrap();
    assert!(command.contains(" hook copilot "));
}

#[test]
fn write_opencode_plugin_installs_packet28_rewrite_plugin() {
    let dir = tempdir().unwrap();
    let path = dir
        .path()
        .join(".config")
        .join("opencode")
        .join("plugins")
        .join("packet28.ts");
    let status = setup_plugins::write_opencode_plugin(&path, true).unwrap();
    assert!(matches!(status, McpConfigStatus::Written));
    let content = fs::read_to_string(&path).unwrap();
    assert!(content.contains("Packet28 rewrite"));
    assert!(content.contains("tool.execute.before"));
    assert!(content.contains("args as Record<string, unknown>).command = rewritten"));

    let status = setup_plugins::write_opencode_plugin(&path, true).unwrap();
    assert!(matches!(status, McpConfigStatus::AlreadyConfigured));
}

#[test]
fn opencode_plugin_smoke_rewrites_and_passes_through_empty_stdout() {
    if std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_err()
    {
        return;
    }

    let dir = tempdir().unwrap();
    let path = dir
        .path()
        .join(".config")
        .join("opencode")
        .join("plugins")
        .join("packet28.ts");
    setup_plugins::write_opencode_plugin(&path, true).unwrap();
    let script = r#"
const fs = require("fs")
let code = fs.readFileSync(process.argv[1], "utf8")
code = code.replace(/^import type .*$/m, "")
code = code.replace("export const Packet28OpenCodePlugin: Plugin =", "const Packet28OpenCodePlugin =")
code = code.replaceAll("(args as Record<string, unknown>)", "args")
code += `
;(async () => {
  const calls = []
  function $(strings, ...values) {
    const rendered = strings.reduce((acc, part, index) => acc + part + (index < values.length ? values[index] : ""), "")
    calls.push({ rendered, values })
    return {
      quiet() { return this },
      nothrow() {
        const command = String(values[0] ?? "")
        if (command === "git status --short") return Promise.resolve({ stdout: "rewritten git status\\n" })
        return Promise.resolve({ stdout: "" })
      },
      then(resolve) { resolve({ stdout: "" }) },
    }
  }
  const plugin = await Packet28OpenCodePlugin({ $ })
  const rewriteArgs = { command: "git status --short" }
  const passthroughArgs = { command: "htop" }
  await plugin["tool.execute.before"]({ tool: "bash" }, { args: rewriteArgs })
  await plugin["tool.execute.before"]({ tool: "shell" }, { args: passthroughArgs })
  console.log(rewriteArgs.command)
  console.log(passthroughArgs.command)
})().catch((err) => { console.error(err); process.exit(1) })
`
eval(code)
"#;
    let output = std::process::Command::new("node")
        .arg("-e")
        .arg(script)
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "node smoke failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "rewritten git status\nhtop\n"
    );
}

#[test]
fn write_hermes_plugin_installs_plugin_and_enables_config() {
    let dir = tempdir().unwrap();
    let status = setup_plugins::write_hermes_plugin(dir.path(), true).unwrap();
    assert!(matches!(status, McpConfigStatus::Written));

    let plugin_dir = crate::runtime_integrations::hermes::plugin_dir(dir.path());
    let init = fs::read_to_string(plugin_dir.join("__init__.py")).unwrap();
    let manifest = fs::read_to_string(plugin_dir.join("plugin.yaml")).unwrap();
    let config =
        fs::read_to_string(crate::runtime_integrations::hermes::config_path(dir.path())).unwrap();
    assert!(init.contains("Packet28 rewrite"));
    assert!(manifest.contains("packet28-rewrite"));
    assert!(setup_plugins::hermes_config_enables_packet28(&config).unwrap());

    let status = setup_plugins::write_hermes_plugin(dir.path(), true).unwrap();
    assert!(matches!(status, McpConfigStatus::AlreadyConfigured));
}

#[test]
#[cfg(unix)]
fn hermes_plugin_smoke_rewrites_and_passes_through_empty_stdout() {
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        return;
    }

    let dir = tempdir().unwrap();
    setup_plugins::write_hermes_plugin(dir.path(), true).unwrap();
    let init = crate::runtime_integrations::hermes::plugin_dir(dir.path()).join("__init__.py");
    let script = r#"
import importlib.util
import subprocess
import sys
spec = importlib.util.spec_from_file_location("packet28_rewrite", sys.argv[1])
mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)
class FakeResult:
    def __init__(self, stdout="", stderr="", returncode=0):
        self.stdout = stdout
        self.stderr = stderr
        self.returncode = returncode
def fake_run(argv, **kwargs):
    assert argv[0:2] == ["Packet28", "rewrite"]
    if argv[2] == "git status --short":
        return FakeResult("rewritten git status\n")
    return FakeResult("")
mod.subprocess.run = fake_run
rewrite_args = {"command": "git status --short"}
mod._pre_tool_call(tool_name="terminal", args=rewrite_args)
passthrough_args = {"command": "htop"}
mod._pre_tool_call(tool_name="terminal", args=passthrough_args)
print(rewrite_args["command"])
print(passthrough_args["command"])
"#;
    let output = std::process::Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(init)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "python smoke failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "rewritten git status\nhtop\n"
    );
}

#[test]
fn patch_hermes_config_preserves_existing_enabled_plugins() {
    let config = setup_plugins::patch_hermes_config(
        r#"
theme: dark
plugins:
  enabled:
    - existing-plugin
"#,
    )
    .unwrap();
    assert!(config.contains("existing-plugin"));
    assert!(setup_plugins::hermes_config_enables_packet28(&config).unwrap());
}

#[test]
fn patch_hermes_config_preserves_order_tags_and_unknown_keys() {
    let original = r#"
theme: dark
workspace: !Packet28
  id: !!str 001
plugins:
  search_path: ./plugins
  enabled:
    - existing-plugin
mode: on
"#;
    let before = yaml_serde::from_str::<yaml_serde::Value>(original).unwrap();

    let patched = setup_plugins::patch_hermes_config(original).unwrap();
    let after = yaml_serde::from_str::<yaml_serde::Value>(&patched).unwrap();

    assert_eq!(after["workspace"], before["workspace"]);
    assert_eq!(
        after["plugins"]["search_path"],
        before["plugins"]["search_path"]
    );
    assert_eq!(after["mode"], before["mode"]);
    assert_eq!(
        after
            .as_mapping()
            .unwrap()
            .keys()
            .filter_map(yaml_serde::Value::as_str)
            .collect::<Vec<_>>(),
        ["theme", "workspace", "plugins", "mode"]
    );
    assert!(setup_plugins::hermes_config_enables_packet28(&patched).unwrap());
}

#[test]
fn write_windsurf_hook_config_installs_packet28_hooks() {
    let dir = tempdir().unwrap();
    let path = dir.path().join(".windsurf").join("hooks.json");
    let status = setup_hooks::write_windsurf_hook_config(&path, dir.path(), true).unwrap();
    assert!(matches!(status, McpConfigStatus::Written));
    let value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert!(value["hooks"]["pre_user_prompt"].is_array());
    assert!(value["hooks"]["pre_run_command"].is_array());
    assert!(value["hooks"]["post_run_command"].is_array());
    assert!(value["hooks"]["post_cascade_response"].is_array());
}

#[test]
fn legacy_generated_claude_continue_relaunch_is_migrated_to_host_managed() {
    let mut config = HookRuntimeConfig {
        relaunch_preference: RelaunchPreference::DaemonManaged,
        relaunch_command: vec!["claude".to_string(), "--continue".to_string()],
        ..HookRuntimeConfig::default()
    };
    let changed = apply_generated_relaunch_command(&mut config);
    assert!(changed);
    assert_eq!(config.relaunch_preference, RelaunchPreference::HostManaged);
    assert!(config.relaunch_command.is_empty());
    assert!(!config.daemon_relaunch_enabled());
}

#[test]
fn legacy_packet28_agent_relaunch_is_migrated_to_host_managed() {
    let mut config = HookRuntimeConfig {
        relaunch_preference: RelaunchPreference::DaemonManaged,
        relaunch_command: vec![
            "/usr/local/bin/packet28-agent".to_string(),
            "--wait-for-handoff".to_string(),
            "--root".to_string(),
            "/tmp/repo".to_string(),
            "--".to_string(),
            "claude".to_string(),
            "--continue".to_string(),
        ],
        ..HookRuntimeConfig::default()
    };
    let changed = apply_generated_relaunch_command(&mut config);
    assert!(changed);
    assert_eq!(config.relaunch_preference, RelaunchPreference::HostManaged);
    assert!(config.relaunch_command.is_empty());
}

#[test]
fn generated_relaunch_preserves_custom_commands() {
    let original = vec!["custom-agent-runner".to_string(), "--resume".to_string()];
    let mut config = HookRuntimeConfig {
        relaunch_preference: RelaunchPreference::DaemonManaged,
        relaunch_command: original.clone(),
        ..HookRuntimeConfig::default()
    };
    let changed = apply_generated_relaunch_command(&mut config);
    assert!(!changed);
    assert_eq!(config.relaunch_command, original);
    assert_eq!(
        config.relaunch_preference,
        RelaunchPreference::DaemonManaged
    );
    assert!(config.daemon_relaunch_enabled());
}

#[test]
fn setup_never_enables_daemon_managed_relaunch_by_default() {
    let mut config = HookRuntimeConfig::default();
    let changed = apply_generated_relaunch_command(&mut config);
    assert!(!changed);
    assert_eq!(config.relaunch_preference, RelaunchPreference::HostManaged);
    assert!(config.relaunch_command.is_empty());
    assert!(!config.daemon_relaunch_enabled());
}

#[test]
fn gitignore_coverage_recognizes_common_spellings() {
    for spelling in [
        ".packet28",
        ".packet28/",
        "/.packet28",
        "/.packet28/",
        ".packet28/*",
        ".packet28/**",
    ] {
        assert!(
            gitignore_covers_packet28_dir(&format!("target/\n{spelling}\n*.log\n")),
            "spelling {spelling} should be recognized as covering .packet28"
        );
    }
    assert!(!gitignore_covers_packet28_dir("target/\n.packet28x/\n"));
    assert!(!gitignore_covers_packet28_dir(""));
}

#[test]
fn ensure_gitignore_is_noop_outside_git_repo() {
    let dir = tempdir().unwrap();
    assert_eq!(ensure_packet28_gitignore(dir.path()).unwrap(), None);
    assert!(!dir.path().join(".gitignore").exists());
}

#[test]
fn ensure_gitignore_creates_entry_in_git_repo() {
    let dir = tempdir().unwrap();
    fs::create_dir(dir.path().join(".git")).unwrap();

    let created = ensure_packet28_gitignore(dir.path()).unwrap();
    assert_eq!(created, Some(dir.path().join(".gitignore")));
    let content = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
    assert!(gitignore_covers_packet28_dir(&content));

    // Idempotent: a second run makes no change and reports nothing added.
    assert_eq!(ensure_packet28_gitignore(dir.path()).unwrap(), None);
    assert_eq!(
        fs::read_to_string(dir.path().join(".gitignore")).unwrap(),
        content
    );
}

#[test]
fn ensure_gitignore_appends_and_preserves_existing_content() {
    let dir = tempdir().unwrap();
    fs::create_dir(dir.path().join(".git")).unwrap();
    // Existing content without a trailing newline must be preserved intact.
    fs::write(dir.path().join(".gitignore"), "target/\n/dist").unwrap();

    let created = ensure_packet28_gitignore(dir.path()).unwrap();
    assert!(created.is_some());
    let content = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
    assert!(content.starts_with("target/\n/dist\n"));
    assert!(gitignore_covers_packet28_dir(&content));
    assert!(content.contains(".packet28/"));
}

#[test]
fn ensure_gitignore_respects_preexisting_coverage() {
    let dir = tempdir().unwrap();
    fs::create_dir(dir.path().join(".git")).unwrap();
    fs::write(dir.path().join(".gitignore"), "/.packet28/\n").unwrap();
    assert_eq!(ensure_packet28_gitignore(dir.path()).unwrap(), None);
}

fn setup_index_status(
    status: &str,
    regex_status: Option<&str>,
    ready: bool,
) -> DaemonIndexStatusResponse {
    DaemonIndexStatusResponse {
        manifest: DaemonIndexManifest {
            status: status.parse().unwrap(),
            generation: 7,
            regex_generation: regex_status.map(|_| 7),
            regex_status: regex_status.map(str::to_string),
            regex_weight_table_version: regex_status.map(|_| 1),
            ..DaemonIndexManifest::default()
        },
        ready,
        ..DaemonIndexStatusResponse::default()
    }
}

#[test]
fn classify_setup_index_status_reports_ready_when_regex_index_is_usable() {
    let dir = tempdir().unwrap();
    let regex_dir = dir.path().join(".packet28").join("index").join("regex-v1");
    fs::create_dir_all(&regex_dir).unwrap();
    fs::write(regex_dir.join("manifest.json"), "{}").unwrap();
    let response = setup_index_status("ready", Some("ready"), true);

    assert!(matches!(
        classify_setup_index_status(dir.path(), &response, false),
        SetupIndexVerification::Ready(_)
    ));
}

#[test]
fn classify_setup_index_status_reports_building_while_index_is_in_progress() {
    let dir = tempdir().unwrap();
    let response = setup_index_status("building", Some("building"), false);

    assert!(matches!(
        classify_setup_index_status(dir.path(), &response, false),
        SetupIndexVerification::Building(_)
    ));
}

#[test]
fn setup_defers_dirty_git_index_without_masking_corruption() {
    let dir = tempdir().unwrap();
    let mut response = setup_index_status("queued", Some("building"), false);
    response.manifest.last_error = Some(
        "index publication failed: full regex index rebuild requires a clean Git working tree"
            .to_string(),
    );
    assert!(matches!(
        classify_setup_index_status(dir.path(), &response, true),
        SetupIndexVerification::Deferred
    ));
    response.manifest.regex_status = Some("corrupt".to_string());
    assert!(matches!(
        classify_setup_index_status(dir.path(), &response, false),
        SetupIndexVerification::Failed { .. }
    ));
}

#[test]
fn classify_setup_index_status_reports_failure_when_regex_artifacts_are_missing_after_timeout() {
    let dir = tempdir().unwrap();
    let response = setup_index_status("building", Some("building"), false);

    match classify_setup_index_status(dir.path(), &response, true) {
        SetupIndexVerification::Failed { reason, .. } => {
            assert!(reason.contains("regex trigram index artifacts are missing"));
        }
        other => panic!("expected failed setup classification, got {other:?}"),
    }
}

#[test]
fn classify_setup_index_status_reports_failure_when_repo_index_claims_ready_without_regex() {
    let dir = tempdir().unwrap();
    let response = setup_index_status("ready", Some("building"), false);

    match classify_setup_index_status(dir.path(), &response, false) {
        SetupIndexVerification::Failed { reason, .. } => {
            assert!(reason.contains("regex trigram index is not ready"));
        }
        other => panic!("expected failed setup classification, got {other:?}"),
    }
}
