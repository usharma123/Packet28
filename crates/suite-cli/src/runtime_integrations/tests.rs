use std::cell::Cell;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use tempfile::tempdir;

use super::*;
use crate::cmd_setup::setup_commands::resolve_packet28_mcp_command;

type CatalogSnapshot = (
    &'static str,
    &'static str,
    Vec<(String, AgentPromptFormat)>,
    Option<Vec<String>>,
    Option<Vec<String>>,
    bool,
);

fn no_commands(_name: &str) -> bool {
    false
}

fn no_command_runs(_name: &str, _args: &[String]) -> Result<bool> {
    Ok(false)
}

fn normalize_path(path: &Path, root: &Path, home: &Path) -> String {
    if let Ok(relative) = path.strip_prefix(root) {
        return format!("R/{}", relative.display());
    }
    if let Ok(relative) = path.strip_prefix(home) {
        return format!("H/{}", relative.display());
    }
    path.display().to_string()
}

fn catalog_snapshot(environment: &RuntimeEnvironment<'_>) -> Vec<CatalogSnapshot> {
    adapters()
        .iter()
        .map(|adapter| {
            let prompts = adapter
                .prompt_targets(environment)
                .into_iter()
                .map(|target| {
                    (
                        normalize_path(&target.path, environment.root(), environment.home()),
                        target.format,
                    )
                })
                .collect();
            let mcp = adapter.mcp.map(|action| {
                action
                    .artifacts(environment)
                    .into_iter()
                    .map(|path| normalize_path(&path, environment.root(), environment.home()))
                    .collect()
            });
            let hooks = adapter.hooks.map(|action| {
                action
                    .artifacts(environment)
                    .into_iter()
                    .map(|path| normalize_path(&path, environment.root(), environment.home()))
                    .collect()
            });
            (
                adapter.name,
                adapter.slug,
                prompts,
                mcp,
                hooks,
                adapter.writes_hook_runtime_config,
            )
        })
        .collect()
}

#[test]
fn catalog_contains_exactly_the_supported_runtimes_in_stable_order() {
    let actual = adapters()
        .iter()
        .map(|adapter| (adapter.name, adapter.slug))
        .collect::<Vec<_>>();

    assert_eq!(
        actual,
        vec![
            ("Claude Code", "claude"),
            ("Cursor", "cursor"),
            ("Codex", "codex"),
            ("Windsurf", "windsurf"),
            ("GitHub Copilot", "copilot"),
            ("Gemini CLI", "gemini"),
            ("OpenCode", "opencode"),
            ("Hermes", "hermes"),
            ("Cline", "cline"),
            ("Roo Code", "roo"),
            ("Kilo Code", "kilocode"),
            ("Google Antigravity", "antigravity"),
        ]
    );
}

#[test]
fn catalog_names_slugs_and_lookups_are_unique() {
    let names = adapters()
        .iter()
        .map(|adapter| adapter.name)
        .collect::<BTreeSet<_>>();
    let slugs = adapters()
        .iter()
        .map(|adapter| adapter.slug)
        .collect::<BTreeSet<_>>();

    assert_eq!(names.len(), adapters().len());
    assert_eq!(slugs.len(), adapters().len());
    assert!(slugs.iter().all(|slug| adapter_for_slug(slug).is_some()));
}

#[test]
fn catalog_paths_formats_and_capabilities_match_contract() {
    let root = tempdir().unwrap();
    let home = tempdir().unwrap();
    let environment =
        RuntimeEnvironment::new(root.path(), home.path(), &no_commands, &no_command_runs);

    let actual = catalog_snapshot(&environment);

    assert_eq!(
        actual,
        vec![
            (
                "Claude Code",
                "claude",
                vec![("R/CLAUDE.md".to_string(), AgentPromptFormat::Claude)],
                Some(vec!["R/.mcp.json".to_string()]),
                Some(vec![
                    "R/.claude/settings.json".to_string(),
                    "R/.packet28/daemon/hook-runtime-v1.json".to_string(),
                ]),
                true,
            ),
            (
                "Cursor",
                "cursor",
                vec![(
                    "R/.cursor/rules/packet28.mdc".to_string(),
                    AgentPromptFormat::CursorRule,
                )],
                Some(vec!["R/.cursor/mcp.json".to_string()]),
                Some(vec!["R/.cursor/hooks.json".to_string()]),
                false,
            ),
            (
                "Codex",
                "codex",
                vec![("R/AGENTS.md".to_string(), AgentPromptFormat::Agents)],
                Some(vec!["H/.codex/config.toml".to_string()]),
                Some(vec!["R/.codex/hooks.json".to_string()]),
                true,
            ),
            (
                "Windsurf",
                "windsurf",
                vec![(
                    "R/.windsurf/rules/packet28.md".to_string(),
                    AgentPromptFormat::WindsurfRule,
                )],
                Some(vec!["H/.codeium/windsurf/mcp_config.json".to_string(),]),
                Some(vec!["R/.windsurf/hooks.json".to_string()]),
                false,
            ),
            (
                "GitHub Copilot",
                "copilot",
                vec![(
                    "R/.github/copilot-instructions.md".to_string(),
                    AgentPromptFormat::Agents,
                )],
                None,
                Some(vec!["R/.github/hooks/packet28-rewrite.json".to_string(),]),
                false,
            ),
            (
                "Gemini CLI",
                "gemini",
                vec![("R/GEMINI.md".to_string(), AgentPromptFormat::Agents)],
                None,
                Some(vec!["H/.gemini/settings.json".to_string()]),
                false,
            ),
            (
                "OpenCode",
                "opencode",
                vec![("R/AGENTS.md".to_string(), AgentPromptFormat::Agents)],
                None,
                Some(vec!["H/.config/opencode/plugins/packet28.ts".to_string(),]),
                false,
            ),
            (
                "Hermes",
                "hermes",
                vec![("R/AGENTS.md".to_string(), AgentPromptFormat::Agents)],
                None,
                Some(vec![
                    "H/.hermes/plugins/packet28-rewrite/__init__.py".to_string(),
                    "H/.hermes/plugins/packet28-rewrite/plugin.yaml".to_string(),
                    "H/.hermes/config.yaml".to_string(),
                ]),
                false,
            ),
            (
                "Cline",
                "cline",
                vec![("R/.clinerules".to_string(), AgentPromptFormat::Agents)],
                None,
                None,
                false,
            ),
            (
                "Roo Code",
                "roo",
                vec![(
                    "R/.roo/rules/packet28.md".to_string(),
                    AgentPromptFormat::Agents,
                )],
                None,
                None,
                false,
            ),
            (
                "Kilo Code",
                "kilocode",
                vec![(
                    "R/.kilocode/rules/packet28-rules.md".to_string(),
                    AgentPromptFormat::Agents,
                )],
                None,
                None,
                false,
            ),
            (
                "Google Antigravity",
                "antigravity",
                vec![(
                    "R/.agents/rules/antigravity-packet28-rules.md".to_string(),
                    AgentPromptFormat::Agents,
                )],
                None,
                None,
                false,
            ),
        ]
    );
}

#[test]
fn cursor_adds_legacy_prompt_only_when_the_file_exists() {
    let root = tempdir().unwrap();
    let home = tempdir().unwrap();
    let environment =
        RuntimeEnvironment::new(root.path(), home.path(), &no_commands, &no_command_runs);
    let cursor = adapter_for_slug("cursor").unwrap();
    assert_eq!(cursor.prompt_targets(&environment).len(), 1);

    fs::write(root.path().join(".cursorrules"), "legacy").unwrap();

    let targets = cursor.prompt_targets(&environment);
    assert_eq!(targets.len(), 2);
    assert_eq!(targets[1].format, AgentPromptFormat::Cursor);
}

#[test]
fn detection_is_false_in_an_empty_injected_environment() {
    let root = tempdir().unwrap();
    let home = tempdir().unwrap();
    let environment =
        RuntimeEnvironment::new(root.path(), home.path(), &no_commands, &no_command_runs);

    let detected = adapters()
        .iter()
        .filter(|adapter| adapter.detected(&environment))
        .map(|adapter| adapter.slug)
        .collect::<Vec<_>>();

    assert!(detected.is_empty(), "unexpected detections: {detected:?}");
}

#[test]
fn detection_uses_only_declared_filesystem_markers() {
    let root = tempdir().unwrap();
    let home = tempdir().unwrap();
    for path in [
        home.path().join(".claude"),
        home.path().join(".cursor"),
        home.path().join(".codeium").join("windsurf"),
        home.path().join(".gemini"),
        home.path().join(".config").join("opencode"),
        home.path().join(".hermes"),
        home.path().join(".cline"),
        home.path().join(".roo"),
        home.path().join(".kilocode"),
        root.path().join(".agents"),
    ] {
        fs::create_dir_all(path).unwrap();
    }
    let copilot = root.path().join(".github").join("copilot-instructions.md");
    fs::create_dir_all(copilot.parent().unwrap()).unwrap();
    fs::write(copilot, "instructions").unwrap();
    let environment =
        RuntimeEnvironment::new(root.path(), home.path(), &no_commands, &no_command_runs);

    let detected = adapters()
        .iter()
        .filter(|adapter| adapter.detected(&environment))
        .map(|adapter| adapter.slug)
        .collect::<Vec<_>>();

    assert_eq!(
        detected,
        vec![
            "claude",
            "cursor",
            "windsurf",
            "copilot",
            "gemini",
            "opencode",
            "hermes",
            "cline",
            "roo",
            "kilocode",
            "antigravity",
        ]
    );
}

#[test]
fn detection_uses_only_declared_commands() {
    let root = tempdir().unwrap();
    let home = tempdir().unwrap();
    let installed = BTreeSet::from([
        "claude",
        "cursor",
        "codex",
        "windsurf",
        "gemini",
        "opencode",
        "hermes",
        "kilocode",
        "antigravity",
    ]);
    let command_exists = |name: &str| installed.contains(name);
    let environment =
        RuntimeEnvironment::new(root.path(), home.path(), &command_exists, &no_command_runs);

    let detected = adapters()
        .iter()
        .filter(|adapter| adapter.detected(&environment))
        .map(|adapter| adapter.slug)
        .collect::<Vec<_>>();

    assert_eq!(
        detected,
        vec![
            "claude",
            "cursor",
            "codex",
            "windsurf",
            "gemini",
            "opencode",
            "hermes",
            "kilocode",
            "antigravity",
        ]
    );
}

fn artifact_bytes(action: IntegrationAction, environment: &RuntimeEnvironment<'_>) -> Vec<Vec<u8>> {
    action
        .artifacts(environment)
        .iter()
        .map(|path| fs::read(path).unwrap())
        .collect()
}

#[test]
fn all_configuration_actions_are_idempotent_and_status_consistent() {
    let root = tempdir().unwrap();
    let home = tempdir().unwrap();
    let environment =
        RuntimeEnvironment::new(root.path(), home.path(), &no_commands, &no_command_runs);
    let mut configured_actions = 0;

    for adapter in adapters() {
        for action in [adapter.mcp, adapter.hooks].into_iter().flatten() {
            configured_actions += 1;
            assert_eq!(
                action.configure(&environment, true).unwrap(),
                McpConfigStatus::Written,
                "{} should write its initial configuration",
                adapter.slug
            );
            let artifacts = action.artifacts(&environment);
            assert!(
                artifacts.iter().all(|path| path.exists()),
                "{} did not create every declared artifact: {artifacts:?}",
                adapter.slug
            );
            let status = action.status(&environment);
            let primary = artifacts[0].display().to_string();
            assert!(
                status.contains(&primary) || primary.starts_with(&status),
                "{} status does not identify its primary artifact",
                adapter.slug
            );
            let before = artifact_bytes(action, &environment);

            assert_eq!(
                action.configure(&environment, true).unwrap(),
                McpConfigStatus::AlreadyConfigured,
                "{} should recognize its existing configuration",
                adapter.slug
            );
            assert_eq!(
                artifact_bytes(action, &environment),
                before,
                "{} changed bytes during an idempotent configure",
                adapter.slug
            );
        }
    }

    assert_eq!(configured_actions, 12);
}

fn write_fixture(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn optional_artifact_bytes(
    action: IntegrationAction,
    environment: &RuntimeEnvironment<'_>,
) -> Vec<Option<Vec<u8>>> {
    action
        .artifacts(environment)
        .iter()
        .map(|path| path.exists().then(|| fs::read(path).unwrap()))
        .collect()
}

fn assert_action_rejects_without_writes(
    action: IntegrationAction,
    environment: &RuntimeEnvironment<'_>,
) {
    let before = optional_artifact_bytes(action, environment);
    assert!(action.configure(environment, true).is_err());
    assert_eq!(optional_artifact_bytes(action, environment), before);
}

#[test]
fn malformed_json_mcp_configs_are_rejected_without_writes() {
    for slug in ["claude", "cursor", "windsurf"] {
        for malformed in ["{not-json", r#"{"mcpServers":[]}"#] {
            let root = tempdir().unwrap();
            let home = tempdir().unwrap();
            let environment =
                RuntimeEnvironment::new(root.path(), home.path(), &no_commands, &no_command_runs);
            let action = adapter_for_slug(slug).unwrap().mcp.unwrap();
            write_fixture(&action.artifacts(&environment)[0], malformed);

            assert_action_rejects_without_writes(action, &environment);
        }
    }
}

#[test]
fn malformed_json_hook_configs_are_rejected_without_partial_writes() {
    for slug in ["claude", "cursor", "windsurf", "copilot", "gemini"] {
        for malformed in ["{not-json", r#"{"hooks":[]}"#] {
            let root = tempdir().unwrap();
            let home = tempdir().unwrap();
            let environment =
                RuntimeEnvironment::new(root.path(), home.path(), &no_commands, &no_command_runs);
            let action = adapter_for_slug(slug).unwrap().hooks.unwrap();
            write_fixture(&action.artifacts(&environment)[0], malformed);

            assert_action_rejects_without_writes(action, &environment);
        }
    }
}

#[test]
fn malformed_codex_toml_is_rejected_without_writes() {
    for malformed in ["not = [valid", "mcp_servers = []"] {
        let root = tempdir().unwrap();
        let home = tempdir().unwrap();
        let environment =
            RuntimeEnvironment::new(root.path(), home.path(), &no_commands, &no_command_runs);
        let action = adapter_for_slug("codex").unwrap().mcp.unwrap();
        write_fixture(&action.artifacts(&environment)[0], malformed);

        assert_action_rejects_without_writes(action, &environment);
    }
}

#[test]
fn malformed_hermes_yaml_is_rejected_without_partial_writes() {
    for malformed in ["plugins: [", "plugins:\n  enabled: packet28-rewrite\n"] {
        let root = tempdir().unwrap();
        let home = tempdir().unwrap();
        let environment =
            RuntimeEnvironment::new(root.path(), home.path(), &no_commands, &no_command_runs);
        let action = adapter_for_slug("hermes").unwrap().hooks.unwrap();
        let artifacts = action.artifacts(&environment);
        write_fixture(&artifacts[2], malformed);

        assert_action_rejects_without_writes(action, &environment);
    }
}

#[test]
fn successful_codex_command_is_verified_before_reporting_written() {
    let root = tempdir().unwrap();
    let home = tempdir().unwrap();
    let config_path = home.path().join(".codex").join("config.toml");
    let expected_root = root.path().display().to_string();
    let calls = Cell::new(0);
    let command_exists = |name: &str| name == "codex";
    let run_command = |name: &str, args: &[String]| {
        assert_eq!(name, "codex");
        assert!(args.windows(2).any(|pair| pair == ["--toolset", "core"]));
        calls.set(calls.get() + 1);
        write_fixture(
            &config_path,
            &format!(
                "[mcp_servers.packet28]\ncommand = {:?}\nargs = [\"--root\", {:?}]\n",
                resolve_packet28_mcp_command(),
                expected_root
            ),
        );
        Ok(true)
    };
    let environment =
        RuntimeEnvironment::new(root.path(), home.path(), &command_exists, &run_command);
    let action = adapter_for_slug("codex").unwrap().mcp.unwrap();

    assert_eq!(
        action.configure(&environment, true).unwrap(),
        McpConfigStatus::Written
    );
    assert_eq!(calls.get(), 1);
    assert_eq!(
        action.configure(&environment, true).unwrap(),
        McpConfigStatus::AlreadyConfigured
    );
    assert_eq!(calls.get(), 1);
    let config: toml::Value = toml::from_str(&fs::read_to_string(config_path).unwrap()).unwrap();
    let args = config["mcp_servers"]["packet28"]["args"]
        .as_array()
        .unwrap();
    assert_eq!(args.len(), 4);
}

#[test]
fn setup_orchestration_contains_no_runtime_switches_or_literals() {
    let orchestration = [
        ("cmd_setup.rs", include_str!("../cmd_setup.rs")),
        (
            "cmd_setup_render.rs",
            include_str!("../cmd_setup_render.rs"),
        ),
        (
            "cmd_setup_runtime.rs",
            include_str!("../cmd_setup_runtime.rs"),
        ),
    ];
    let forbidden = [
        "RuntimeKind",
        "runtime_supports_mcp",
        "runtime_supports_hooks",
        "runtime_needs_hook_runtime_config",
        "configure_runtime_mcp",
        "configure_runtime_hooks",
        "runtime_mcp_status",
        "runtime_hook_status",
        "write_claude_hook_config",
        "write_cursor_hook_config",
        "write_copilot_hook_config",
        "write_gemini_hook_config",
        "write_windsurf_hook_config",
        "write_opencode_plugin",
        "write_hermes_plugin",
        "\"claude\"",
        "\"cursor\"",
        "\"codex\"",
        "\"windsurf\"",
        "\"copilot\"",
        "\"gemini\"",
        "\"opencode\"",
        "\"hermes\"",
        "\"cline\"",
        "\"roo\"",
        "\"kilocode\"",
        "\"antigravity\"",
        ".mcp.json",
        ".cursor/",
        ".codeium/",
        ".claude/",
        ".gemini/",
        ".hermes/",
        ".clinerules",
        ".kilocode/",
        ".agents/",
        "std::env::var(\"HOME\")",
        "Command::new(\"which\")",
    ];

    for (name, source) in orchestration {
        for needle in &forbidden {
            assert!(
                !source.contains(needle),
                "{name} contains forbidden runtime-specific orchestration token {needle:?}"
            );
        }
    }
}
