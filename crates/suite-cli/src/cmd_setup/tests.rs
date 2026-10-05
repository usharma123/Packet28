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
fn claude_hook_merge_preserves_mixed_http_handlers_and_entry_metadata() {
    let dir = tempdir().unwrap();
    let path = dir.path().join(".claude/settings.json");
    setup_hooks::write_claude_hook_config(&path, dir.path(), true).unwrap();
    let generated: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let owned = generated["hooks"]["PreToolUse"][0]["hooks"][0].clone();
    let endpoint = owned["url"].as_str().unwrap();
    let token = owned["headers"]["X-Packet28-Hook-Token"].as_str().unwrap();
    let user_handlers = vec![
        json!({"type": "command", "command": "user-audit"}),
        json!({"type": "http", "url": format!("{endpoint}?user=1"), "headers": {"X-Packet28-Hook-Token": token}}),
        json!({"type": "http", "url": "https://user.example/packet28/claude-hook", "headers": {"X-Packet28-Hook-Token": token}}),
        json!({"type": "http", "url": endpoint}),
        json!({"type": "http", "url": endpoint, "headers": {"X-Packet28-Hook-Token": "user-token"}}),
    ];
    let mut mixed = user_handlers.clone();
    mixed.insert(1, owned);
    fs::write(
        &path,
        serde_json::to_vec(&json!({
            "env": {"KEEP": "yes"},
            "allowedHttpHookUrls": ["https://user.example/hooks"],
            "hooks": {"PreToolUse": [{"matcher": "Bash", "userMetadata": "keep", "hooks": mixed}]}
        }))
        .unwrap(),
    )
    .unwrap();
    setup_hooks::write_claude_hook_config(&path, dir.path(), true).unwrap();
    let merged: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(merged["env"]["KEEP"], "yes");
    let preserved = &merged["hooks"]["PreToolUse"][0];
    assert_eq!(preserved["matcher"], "Bash");
    assert_eq!(preserved["userMetadata"], "keep");
    assert_eq!(preserved["hooks"], json!(user_handlers));
    assert!(merged["allowedHttpHookUrls"]
        .as_array()
        .unwrap()
        .contains(&json!("https://user.example/hooks")));
    assert_eq!(merged["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
    let status = setup_hooks::write_claude_hook_config(&path, dir.path(), true).unwrap();
    assert!(matches!(status, McpConfigStatus::AlreadyConfigured));
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(&path).unwrap()).unwrap(),
        merged
    );
}

#[test]
fn claude_hook_merge_preserves_user_wrappers_and_migrates_exact_generated_commands() {
    let dir = tempdir().unwrap();
    let path = dir.path().join(".claude/settings.json");
    fs::create_dir(path.parent().unwrap()).unwrap();
    let guarded = super::setup_commands::guarded_packet28_hook_command(
        "/missing/Packet28",
        "claude",
        Path::new("/old/workspace"),
    );
    let direct_operator = "Packet28 hook claude --root /old;user-audit";
    let guarded_operator = format!("{guarded};user-audit");
    assert_eq!(shell_words::split(direct_operator).unwrap().len(), 5);
    assert_eq!(shell_words::split(&guarded_operator).unwrap().len(), 6);
    let user_handlers = vec![
        json!({"type": "command", "command": direct_operator}),
        json!({"type": "command", "command": guarded_operator}),
        json!({"type": "command", "command": "echo Packet28 hook claude --root /user"}),
        json!({"type": "command", "command": "sh -c 'printf %s \"Packet28 hook claude --root user\"'"}),
        json!({"type": "command", "command": "user-audit"}),
    ];
    let mut mixed = user_handlers.clone();
    mixed.push(json!({"type": "command", "command": guarded}));
    mixed.push(json!({"type": "command", "command": "/missing/Packet28 hook claude --root /old/workspace"}));
    fs::write(
        &path,
        serde_json::to_vec(&json!({
            "hooks": {"SessionStart": [{"matcher": "user-matcher", "timeout": 42, "hooks": mixed}]}
        }))
        .unwrap(),
    )
    .unwrap();
    setup_hooks::write_claude_hook_config(&path, dir.path(), true).unwrap();
    let merged: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let entries = merged["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0]["matcher"], "user-matcher");
    assert_eq!(entries[0]["timeout"], 42);
    assert_eq!(entries[0]["hooks"], json!(user_handlers));
    let command = entries[1]["hooks"][0]["command"].as_str().unwrap();
    assert!(command.contains("${CLAUDE_PROJECT_DIR}"));
    assert!(!command.contains("/old/workspace"));
    assert_eq!(entries[2]["hooks"][0]["type"], "http");
}

#[test]
fn generated_hook_command_ownership_requires_exact_outer_shell_serialization() {
    for runtime in ["claude", "codex"] {
        let guarded = super::setup_commands::guarded_packet28_hook_command(
            "/missing/Packet28",
            runtime,
            Path::new("/old workspace"),
        );
        assert!(super::setup_commands::is_generated_packet28_hook_command(
            &guarded, runtime
        ));
        let generated =
            super::setup_commands::generated_packet28_hook_command(runtime, Path::new("/root"));
        assert!(super::setup_commands::is_generated_packet28_hook_command(
            &generated, runtime
        ));
        for suffix in [
            ";user-audit",
            "&&user-audit",
            "|user-audit",
            " >user.log",
            "$(user-audit)",
            "`user-audit`",
        ] {
            assert!(
                !super::setup_commands::is_generated_packet28_hook_command(
                    &format!("{guarded}{suffix}"),
                    runtime
                ),
                "suffix {suffix:?}"
            );
        }
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
fn write_hook_runtime_config_re_enables_stale_kill_switch() {
    let dir = tempdir().unwrap();
    let path = packet28_daemon_protocol::paths::hook_runtime_config_path(dir.path());
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    // Simulate the kill switch left engaged by a prior `packet28 uninstall`,
    // while HTTP hook settings are already present so only `hooks_enabled`
    // needs fixing.
    let disabled = HookRuntimeConfig {
        hooks_enabled: false,
        http_hook_port: Some(45123),
        http_hook_token: Some("existing-token".to_string()),
        ..HookRuntimeConfig::default()
    };
    fs::write(
        &path,
        format!("{}\n", serde_json::to_string_pretty(&disabled).unwrap()),
    )
    .unwrap();

    let status = write_hook_runtime_config(dir.path(), true).unwrap();
    assert!(matches!(status, McpConfigStatus::Written));

    let written: HookRuntimeConfig =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert!(
        written.hooks_enabled,
        "setup must re-enable hook ingest when configuring a hook runtime"
    );
    // Existing HTTP settings are preserved rather than regenerated.
    assert_eq!(written.http_hook_port, Some(45123));
    assert_eq!(written.http_hook_token.as_deref(), Some("existing-token"));
}

#[cfg(unix)]
#[test]
fn write_hook_runtime_config_keeps_private_token_in_traversable_directories() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempdir().unwrap();
    let state_dir = dir.path().join(".packet28");
    let daemon_dir = state_dir.join("daemon");
    fs::create_dir_all(&daemon_dir).unwrap();
    for parent in [dir.path(), state_dir.as_path(), daemon_dir.as_path()] {
        fs::set_permissions(parent, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = packet28_daemon_protocol::paths::hook_runtime_config_path(dir.path());
    let config = HookRuntimeConfig {
        hooks_enabled: false,
        http_hook_port: Some(45123),
        http_hook_token: Some("synthetic-private-token".to_string()),
        ..HookRuntimeConfig::default()
    };
    fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

    assert!(matches!(
        write_hook_runtime_config(dir.path(), true).unwrap(),
        McpConfigStatus::Written
    ));

    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let written: HookRuntimeConfig = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert!(written.hooks_enabled);
    assert_eq!(written.http_hook_token, config.http_hook_token);
    // Privacy must come from the file, even when existing ancestors are public.
    for parent in [dir.path(), state_dir.as_path(), daemon_dir.as_path()] {
        assert_eq!(
            fs::metadata(parent).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
}

#[cfg(unix)]
#[test]
fn claude_settings_create_private_token_copy_in_traversable_directories() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempdir().unwrap();
    let parent = dir.path().join(".claude");
    fs::create_dir(&parent).unwrap();
    for directory in [dir.path(), parent.as_path()] {
        fs::set_permissions(directory, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = parent.join("settings.json");
    setup_hooks::write_claude_hook_config(&path, dir.path(), true).unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let settings: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let runtime: HookRuntimeConfig = serde_json::from_slice(
        &fs::read(packet28_daemon_protocol::paths::hook_runtime_config_path(
            dir.path(),
        ))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        settings["hooks"]["PreToolUse"][0]["hooks"][0]["headers"]["X-Packet28-Hook-Token"].as_str(),
        runtime.http_hook_token.as_deref(),
    );
    for directory in [dir.path(), parent.as_path()] {
        assert_eq!(
            fs::metadata(directory).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
}

#[cfg(unix)]
#[test]
fn claude_settings_rewrite_private_and_public_files_without_losing_user_handlers() {
    use std::os::unix::fs::PermissionsExt;

    for mode in [0o600, 0o644] {
        let dir = tempdir().unwrap();
        let parent = dir.path().join(".claude");
        fs::create_dir(&parent).unwrap();
        for directory in [dir.path(), parent.as_path()] {
            fs::set_permissions(directory, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = parent.join("settings.json");
        let user_handler = json!({
            "matcher": "Bash",
            "hooks": [{"type": "command", "command": "user-audit"}]
        });
        fs::write(
            &path,
            serde_json::to_vec(&json!({
                "theme": "dark",
                "hooks": {"PreToolUse": [user_handler.clone()]}
            }))
            .unwrap(),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        setup_hooks::write_claude_hook_config(&path, dir.path(), true).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let settings: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(settings["theme"], "dark");
        assert_eq!(settings["hooks"]["PreToolUse"][0], user_handler);
        assert_eq!(
            fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
}

#[cfg(unix)]
#[test]
fn claude_settings_normalize_existing_token_copy_permissions_without_changing_bytes() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempdir().unwrap();
    let path = dir.path().join(".claude/settings.json");
    setup_hooks::write_claude_hook_config(&path, dir.path(), true).unwrap();
    let original = fs::read(&path).unwrap();
    for directory in [dir.path(), path.parent().unwrap()] {
        fs::set_permissions(directory, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let status = setup_hooks::write_claude_hook_config(&path, dir.path(), true).unwrap();
    assert!(matches!(status, McpConfigStatus::AlreadyConfigured));
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(fs::read(&path).unwrap(), original);
}

#[cfg(unix)]
#[test]
fn claude_settings_reject_linked_token_files_before_initializing_runtime() {
    use std::os::unix::fs::symlink;

    for hard_link in [false, true] {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let path = dir.path().join(".claude/settings.json");
        fs::create_dir(path.parent().unwrap()).unwrap();
        let original = br#"{"theme":"dark"}"#;
        let outside_path = outside.path().join("settings.json");
        fs::write(&outside_path, original).unwrap();
        if hard_link {
            fs::hard_link(&outside_path, &path).unwrap();
        } else {
            symlink(&outside_path, &path).unwrap();
        }
        assert!(setup_hooks::write_claude_hook_config(&path, dir.path(), true).is_err());
        assert_eq!(fs::read(&outside_path).unwrap(), original);
        assert!(!packet28_daemon_protocol::paths::hook_runtime_config_path(dir.path()).exists());
    }
}

#[cfg(unix)]
#[test]
fn claude_settings_reject_linked_parent_before_initializing_runtime() {
    use std::os::unix::fs::symlink;

    let dir = tempdir().unwrap();
    let outside = tempdir().unwrap();
    symlink(outside.path(), dir.path().join(".claude")).unwrap();
    assert!(setup_hooks::write_claude_hook_config(
        &dir.path().join(".claude/settings.json"),
        dir.path(),
        true,
    )
    .is_err());
    assert!(!outside.path().join("settings.json").exists());
    assert!(!packet28_daemon_protocol::paths::hook_runtime_config_path(dir.path()).exists());
}

#[test]
fn claude_settings_reject_paths_outside_workspace() {
    let dir = tempdir().unwrap();
    let outside = tempdir().unwrap();
    let path = outside.path().join("settings.json");
    let original = br#"{"theme":"dark"}"#;
    fs::write(&path, original).unwrap();
    assert!(setup_hooks::write_claude_hook_config(&path, dir.path(), true).is_err());
    assert_eq!(fs::read(&path).unwrap(), original);
    assert!(!packet28_daemon_protocol::paths::hook_runtime_config_path(dir.path()).exists());
}

#[cfg(unix)]
#[test]
fn claude_setup_creates_private_runtime_config_in_traversable_directories() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempdir().unwrap();
    let state_dir = dir.path().join(".packet28");
    let daemon_dir = state_dir.join("daemon");
    fs::create_dir_all(&daemon_dir).unwrap();
    for parent in [dir.path(), state_dir.as_path(), daemon_dir.as_path()] {
        fs::set_permissions(parent, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let settings = dir.path().join(".claude/settings.json");
    setup_hooks::write_claude_hook_config(&settings, dir.path(), true).unwrap();

    let path = packet28_daemon_protocol::paths::hook_runtime_config_path(dir.path());
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let written: HookRuntimeConfig = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert!(written.http_hook_token.is_some());
    assert_eq!(
        fs::metadata(&daemon_dir).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

#[test]
fn claude_http_settings_preserve_disabled_ingest_until_hook_opt_in() {
    let dir = tempdir().unwrap();
    let path = packet28_daemon_protocol::paths::hook_runtime_config_path(dir.path());
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        serde_json::to_vec(&HookRuntimeConfig {
            hooks_enabled: false,
            ..HookRuntimeConfig::default()
        })
        .unwrap(),
    )
    .unwrap();

    setup_hooks::write_claude_hook_config(
        &dir.path().join(".claude/settings.json"),
        dir.path(),
        true,
    )
    .unwrap();
    let initialized: HookRuntimeConfig = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert!(!initialized.hooks_enabled);
    assert!(initialized.http_hook_token.is_some());
    let unchanged = fs::read(&path).unwrap();
    assert!(matches!(
        write_hook_runtime_config(dir.path(), false).unwrap(),
        McpConfigStatus::Declined
    ));
    assert_eq!(fs::read(&path).unwrap(), unchanged);

    write_hook_runtime_config(dir.path(), true).unwrap();
    let enabled: HookRuntimeConfig = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert!(enabled.hooks_enabled);
    assert_eq!(enabled.http_hook_token, initialized.http_hook_token);
    assert_eq!(enabled.http_hook_port, initialized.http_hook_port);
}

#[cfg(unix)]
#[test]
fn write_hook_runtime_config_rejects_linked_token_files() {
    use std::os::unix::fs::symlink;

    for hard_link in [false, true] {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let path = packet28_daemon_protocol::paths::hook_runtime_config_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = serde_json::to_vec(&HookRuntimeConfig {
            hooks_enabled: false,
            ..HookRuntimeConfig::default()
        })
        .unwrap();
        let outside_path = outside.path().join("runtime.json");
        fs::write(&outside_path, &original).unwrap();
        if hard_link {
            fs::hard_link(&outside_path, &path).unwrap();
        } else {
            symlink(&outside_path, &path).unwrap();
        }

        assert!(write_hook_runtime_config(dir.path(), true).is_err());
        assert_eq!(fs::read(&outside_path).unwrap(), original);
    }
}

/// Creates an index-status fixture with the specified manifest and readiness values.
///
/// # Examples
///
/// ```
/// let status = setup_index_status("ready", None, true);
/// assert!(status.ready);
/// assert!(status.manifest.regex_status.is_none());
/// ```
///
/// # Arguments
///
/// * `status` - Manifest status to parse into the fixture.
/// * `regex_status` - Optional regex index status and associated metadata.
/// * `ready` - Whether the overall index is ready.
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
