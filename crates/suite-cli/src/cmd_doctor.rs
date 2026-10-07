use std::fs;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::Args;
use packet28_daemon_protocol::index::{DaemonIndexState, DaemonIndexStatusRequest};
use packet28_daemon_protocol::message::{DaemonRequest, DaemonResponse};
use serde::Serialize;
use serde_json::Value;

use crate::runtime_integrations::{
    antigravity, claude, cline, copilot, cursor, gemini, hermes, kilocode, opencode, roo, windsurf,
};

#[path = "cmd_doctor_codex.rs"]
mod doctor_codex;
#[path = "cmd_doctor_mcp.rs"]
mod doctor_mcp;
use doctor_mcp::check_mcp_round_trip;

#[derive(Args)]
pub struct DoctorArgs {
    #[arg(long, default_value = ".")]
    pub root: String,

    /// Limit runtime-specific checks to one agent
    #[arg(long)]
    pub agent: Option<String>,

    #[arg(long)]
    pub json: bool,

    #[arg(long)]
    pub pretty: bool,
}

#[derive(Debug, Clone, Serialize)]
struct DoctorCheck {
    name: &'static str,
    ok: bool,
    required: bool,
    detail: String,
}

#[derive(Debug, Serialize)]
struct McpConfigCheck {
    path: String,
    exists: bool,
    packet28_configured: bool,
    detail: String,
}

#[derive(Debug, Serialize)]
struct DoctorReport {
    root: String,
    ok: bool,
    daemon: DoctorCheck,
    index: DoctorCheck,
    mcp_config: Vec<McpConfigCheck>,
    handshake: DoctorCheck,
    reducer_round_trip: DoctorCheck,
    push_notifications: DoctorCheck,
    handoff_round_trip: DoctorCheck,
    checks: Vec<DoctorCheck>,
}

pub fn run(args: DoctorArgs) -> Result<i32> {
    let root = crate::cmd_daemon::resolve_root_arg(&args.root);
    let report = build_report(&root, args.agent.as_deref());
    if args.json {
        let text = if args.pretty {
            serde_json::to_string_pretty(&report)?
        } else {
            serde_json::to_string(&report)?
        };
        println!("{text}");
    } else {
        print_human_report(&report);
    }
    Ok(if report.ok { 0 } else { 1 })
}

fn build_report(root: &Path, agent: Option<&str>) -> DoctorReport {
    if matches!(agent, Some("copilot")) {
        return build_copilot_report(root);
    }
    if matches!(agent, Some("cursor")) {
        return build_cursor_report(root);
    }
    if matches!(agent, Some("gemini")) {
        return build_gemini_report(root);
    }
    if matches!(agent, Some("opencode")) {
        return build_opencode_report(root);
    }
    if matches!(agent, Some("hermes")) {
        return build_hermes_report(root);
    }
    if matches!(agent, Some("windsurf")) {
        return build_windsurf_report(root);
    }
    if let Some(agent) = agent {
        if let Some(target) = instruction_only_agent_target(root, agent) {
            return build_instruction_only_agent_report(root, target);
        }
    }
    let daemon = check_daemon(root);
    let index = check_index(root);
    let mcp_config = if matches!(agent, Some("codex")) {
        vec![doctor_codex::mcp_config(root, &dirs_home())]
    } else {
        collect_mcp_config_checks(root)
    };
    let mcp_config_summary = summarize_mcp_config(root, &mcp_config);
    let mcp_round_trip = if matches!(agent, Some("codex")) {
        doctor_mcp::check_mcp_round_trip_for_runtime(root, "codex")
    } else {
        check_mcp_round_trip(root)
    };
    let experiment_manifest = check_experiment_manifest(root);
    let mut checks = vec![
        daemon.clone(),
        index.clone(),
        mcp_config_summary,
        mcp_round_trip.handshake.clone(),
        mcp_round_trip.reducer_round_trip.clone(),
        mcp_round_trip.push_notifications.clone(),
        mcp_round_trip.handoff_round_trip.clone(),
        experiment_manifest,
    ];
    if matches!(agent, Some("claude")) {
        checks.insert(2, check_claude_hook_config(root));
        checks.insert(3, check_claude_hook_service(root));
    }
    if matches!(agent, Some("codex")) {
        checks.extend(doctor_codex::hook_checks(root));
        checks.push(check_instruction_file(
            "codex",
            "Codex",
            &crate::runtime_integrations::codex::prompt_path(root),
        ));
    }
    let ok = checks
        .iter()
        .filter(|check| check.required)
        .all(|check| check.ok);
    DoctorReport {
        root: root.display().to_string(),
        ok,
        daemon,
        index,
        mcp_config,
        handshake: mcp_round_trip.handshake,
        reducer_round_trip: mcp_round_trip.reducer_round_trip,
        push_notifications: mcp_round_trip.push_notifications,
        handoff_round_trip: mcp_round_trip.handoff_round_trip,
        checks,
    }
}

fn check_claude_hook_config(root: &Path) -> DoctorCheck {
    let path = claude::settings_path(root);
    let result = (|| -> Result<String> {
        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read '{}'", path.display()))?;
        let value: Value = serde_json::from_str(&content)
            .with_context(|| format!("invalid JSON in '{}'", path.display()))?;
        let runtime_path = packet28_daemon_protocol::paths::hook_runtime_config_path(root);
        let runtime_content = fs::read_to_string(&runtime_path)
            .with_context(|| format!("failed to read '{}'", runtime_path.display()))?;
        let runtime_config: packet28_daemon_protocol::hooks::HookRuntimeConfig =
            serde_json::from_str(&runtime_content)
                .with_context(|| format!("invalid JSON in '{}'", runtime_path.display()))?;
        let port = runtime_config
            .http_hook_port
            .ok_or_else(|| anyhow!("hook runtime is missing http_hook_port"))?;
        let token = runtime_config
            .http_hook_token
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow!("hook runtime is missing http_hook_token"))?;
        let expected_url = format!("http://127.0.0.1:{port}/packet28/claude-hook");

        for event in ["SessionStart", "UserPromptSubmit"] {
            let entries = value
                .pointer(&format!("/hooks/{event}"))
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("missing hooks.{event} array"))?;
            let configured = entries.iter().any(|entry| {
                entry
                    .get("hooks")
                    .and_then(Value::as_array)
                    .is_some_and(|hooks| {
                        hooks.iter().any(|hook| {
                            hook.get("type").and_then(Value::as_str) == Some("command")
                                && hook.get("command").and_then(Value::as_str).is_some_and(
                                    |command| {
                                        command.contains(" hook claude ")
                                            && (command.contains("${CLAUDE_PROJECT_DIR}")
                                                || command.contains(&root.display().to_string()))
                                    },
                                )
                        })
                    })
            });
            if !configured {
                return Err(anyhow!(
                    "Packet28 {event} command hook is missing or targets another workspace"
                ));
            }
        }

        for event in [
            "PreToolUse",
            "PostToolUse",
            "PostToolUseFailure",
            "PreCompact",
            "SessionEnd",
            "Stop",
            "SubagentStop",
        ] {
            let entries = value
                .pointer(&format!("/hooks/{event}"))
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("missing hooks.{event} array"))?;
            let configured = entries.iter().any(|entry| {
                entry
                    .get("hooks")
                    .and_then(Value::as_array)
                    .is_some_and(|hooks| {
                        hooks.iter().any(|hook| {
                            hook.get("type").and_then(Value::as_str) == Some("http")
                                && hook.get("url").and_then(Value::as_str)
                                    == Some(expected_url.as_str())
                                && hook
                                    .pointer("/headers/X-Packet28-Hook-Token")
                                    .and_then(Value::as_str)
                                    == Some(token)
                        })
                    })
            });
            if !configured {
                return Err(anyhow!(
                    "Packet28 {event} HTTP hook does not match the local hook runtime"
                ));
            }
        }

        let allowed = value
            .get("allowedHttpHookUrls")
            .and_then(Value::as_array)
            .is_some_and(|urls| urls.iter().any(|url| url.as_str() == Some(&expected_url)));
        if !allowed {
            return Err(anyhow!(
                "allowedHttpHookUrls does not include the Packet28 hook endpoint"
            ));
        }

        Ok(format!(
            "Claude hooks match the local Packet28 runtime at {}",
            path.display()
        ))
    })();
    match result {
        Ok(detail) => DoctorCheck {
            name: "claude_hook_config",
            ok: true,
            required: true,
            detail,
        },
        Err(error) => DoctorCheck {
            name: "claude_hook_config",
            ok: false,
            required: true,
            detail: format!("claude: {error}"),
        },
    }
}

fn check_claude_hook_service(root: &Path) -> DoctorCheck {
    match crate::cmd_hook_http::check_hook_http_server(root) {
        Ok(()) => DoctorCheck {
            name: "claude_hook_http",
            ok: true,
            required: true,
            detail: format!("Claude HTTP hook service is healthy for {}", root.display()),
        },
        Err(error) => DoctorCheck {
            name: "claude_hook_http",
            ok: false,
            required: true,
            detail: format!("claude: {error}"),
        },
    }
}

fn check_experiment_manifest(root: &Path) -> DoctorCheck {
    let manifest = root.join("docs/experiments/manifest.json");
    if !manifest.exists() {
        return DoctorCheck {
            name: "experiment_manifest",
            ok: false,
            required: false,
            detail: "no docs/experiments/manifest.json found; add one and run `Packet28 verify experiments --root . --json` to audit experiment evidence".to_string(),
        };
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            return DoctorCheck {
                name: "experiment_manifest",
                ok: false,
                required: false,
                detail: format!(
                    "failed to resolve Packet28 binary for manifest verification: {error}"
                ),
            }
        }
    };
    match Command::new(exe)
        .arg("verify")
        .arg("experiments")
        .arg("--root")
        .arg(root)
        .arg("--json")
        .output()
    {
        Ok(output) if output.status.success() => DoctorCheck {
            name: "experiment_manifest",
            ok: true,
            required: false,
            detail: "docs/experiments/manifest.json verified with `Packet28 verify experiments --root . --json`".to_string(),
        },
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            DoctorCheck {
                name: "experiment_manifest",
                ok: false,
                required: false,
                detail: format!(
                    "experiment manifest verification failed: {}{}",
                    stdout.trim(),
                    stderr.trim()
                ),
            }
        }
        Err(error) => DoctorCheck {
            name: "experiment_manifest",
            ok: false,
            required: false,
            detail: format!("failed to run experiment manifest verification: {error}"),
        },
    }
}

struct InstructionOnlyAgentTarget {
    slug: &'static str,
    name: &'static str,
    path: std::path::PathBuf,
}

fn instruction_only_agent_target(root: &Path, agent: &str) -> Option<InstructionOnlyAgentTarget> {
    match agent {
        "cline" => Some(InstructionOnlyAgentTarget {
            slug: "cline",
            name: "Cline",
            path: cline::rules_path(root),
        }),
        "roo" => Some(InstructionOnlyAgentTarget {
            slug: "roo",
            name: "Roo Code",
            path: roo::rules_path(root),
        }),
        "kilocode" => Some(InstructionOnlyAgentTarget {
            slug: "kilocode",
            name: "Kilo Code",
            path: kilocode::rules_path(root),
        }),
        "antigravity" => Some(InstructionOnlyAgentTarget {
            slug: "antigravity",
            name: "Google Antigravity",
            path: antigravity::rules_path(root),
        }),
        _ => None,
    }
}

fn build_copilot_report(root: &Path) -> DoctorReport {
    let daemon = DoctorCheck {
        name: "daemon",
        ok: true,
        required: false,
        detail: "not required for GitHub Copilot project hook verification".to_string(),
    };
    let index = DoctorCheck {
        name: "index",
        ok: true,
        required: false,
        detail: "not required for GitHub Copilot project hook verification".to_string(),
    };
    let mcp_config = Vec::new();
    let instruction_file = check_instruction_file(
        "copilot",
        "GitHub Copilot",
        &copilot::instructions_path(root),
    );
    let hook_config = check_copilot_hook_config(root);
    let handshake = DoctorCheck {
        name: "mcp_handshake",
        ok: true,
        required: false,
        detail: "GitHub Copilot uses a project PreToolUse hook, not Packet28 MCP setup".to_string(),
    };
    let reducer_round_trip = DoctorCheck {
        name: "runtime_capture_support",
        ok: hook_config.ok,
        required: true,
        detail: hook_config.detail.clone(),
    };
    let push_notifications = DoctorCheck {
        name: "push_notifications",
        ok: true,
        required: false,
        detail: "not required for GitHub Copilot project hook verification".to_string(),
    };
    let handoff_round_trip = DoctorCheck {
        name: "handoff_round_trip",
        ok: true,
        required: false,
        detail: "not required for GitHub Copilot project hook verification".to_string(),
    };
    let checks = vec![
        daemon.clone(),
        index.clone(),
        instruction_file,
        hook_config,
        handshake.clone(),
        reducer_round_trip.clone(),
        push_notifications.clone(),
        handoff_round_trip.clone(),
    ];
    let ok = checks
        .iter()
        .filter(|check| check.required)
        .all(|check| check.ok);
    DoctorReport {
        root: root.display().to_string(),
        ok,
        daemon,
        index,
        mcp_config,
        handshake,
        reducer_round_trip,
        push_notifications,
        handoff_round_trip,
        checks,
    }
}

fn build_cursor_report(root: &Path) -> DoctorReport {
    let daemon = check_daemon(root);
    let index = check_index(root);
    let mcp_config = vec![inspect_mcp_config(&cursor::mcp_config_path(root))];
    let mcp_config_summary = summarize_mcp_config(root, &mcp_config);
    let instruction_file = check_instruction_file("cursor", "Cursor", &cursor::rule_path(root));
    let hook_config = check_cursor_hook_config(root);
    let mcp_round_trip = check_mcp_round_trip(root);
    let checks = vec![
        daemon.clone(),
        index.clone(),
        mcp_config_summary,
        instruction_file,
        hook_config,
        mcp_round_trip.handshake.clone(),
        mcp_round_trip.reducer_round_trip.clone(),
        mcp_round_trip.push_notifications.clone(),
        mcp_round_trip.handoff_round_trip.clone(),
    ];
    let ok = checks
        .iter()
        .filter(|check| check.required)
        .all(|check| check.ok);
    DoctorReport {
        root: root.display().to_string(),
        ok,
        daemon,
        index,
        mcp_config,
        handshake: mcp_round_trip.handshake,
        reducer_round_trip: mcp_round_trip.reducer_round_trip,
        push_notifications: mcp_round_trip.push_notifications,
        handoff_round_trip: mcp_round_trip.handoff_round_trip,
        checks,
    }
}

fn build_instruction_only_agent_report(
    root: &Path,
    target: InstructionOnlyAgentTarget,
) -> DoctorReport {
    let daemon = DoctorCheck {
        name: "daemon",
        ok: true,
        required: false,
        detail: format!(
            "not required for {} instruction-file verification",
            target.name
        ),
    };
    let index = DoctorCheck {
        name: "index",
        ok: true,
        required: false,
        detail: format!(
            "not required for {} instruction-file verification",
            target.name
        ),
    };
    let mcp_config = Vec::new();
    let handshake = DoctorCheck {
        name: "mcp_handshake",
        ok: true,
        required: false,
        detail: format!(
            "{} is currently an instruction-file target; Packet28 does not claim MCP setup for this agent",
            target.name
        ),
    };
    let reducer_round_trip = DoctorCheck {
        name: "runtime_rewrite_support",
        ok: true,
        required: false,
        detail: format!(
            "{} is currently guidance-only; no transparent runtime rewrite hook/plugin is claimed",
            target.name
        ),
    };
    let push_notifications = DoctorCheck {
        name: "push_notifications",
        ok: true,
        required: false,
        detail: format!(
            "not required for {} instruction-file verification",
            target.name
        ),
    };
    let handoff_round_trip = DoctorCheck {
        name: "handoff_round_trip",
        ok: true,
        required: false,
        detail: format!(
            "not required for {} instruction-file verification",
            target.name
        ),
    };
    let instruction_file = check_instruction_file(target.slug, target.name, &target.path);
    let checks = vec![
        daemon.clone(),
        index.clone(),
        instruction_file,
        handshake.clone(),
        reducer_round_trip.clone(),
        push_notifications.clone(),
        handoff_round_trip.clone(),
    ];
    let ok = checks
        .iter()
        .filter(|check| check.required)
        .all(|check| check.ok);
    DoctorReport {
        root: root.display().to_string(),
        ok,
        daemon,
        index,
        mcp_config,
        handshake,
        reducer_round_trip,
        push_notifications,
        handoff_round_trip,
        checks,
    }
}

fn check_instruction_file(slug: &str, name: &str, path: &Path) -> DoctorCheck {
    let result = (|| -> Result<String> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read '{}'", path.display()))?;
        if !content.contains("Packet28 Guidance") && !content.contains("Packet28 Integration") {
            return Err(anyhow!(
                "instruction file does not contain Packet28 guidance"
            ));
        }
        Ok(format!(
            "{name} instruction file present at {}",
            path.display()
        ))
    })();
    match result {
        Ok(detail) => DoctorCheck {
            name: "instruction_file",
            ok: true,
            required: true,
            detail,
        },
        Err(err) => DoctorCheck {
            name: "instruction_file",
            ok: false,
            required: true,
            detail: format!("{slug}: {err}"),
        },
    }
}

fn check_copilot_hook_config(root: &Path) -> DoctorCheck {
    let path = copilot::hook_config_path(root);
    let result = (|| -> Result<String> {
        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read '{}'", path.display()))?;
        let value: Value = serde_json::from_str(&content)
            .with_context(|| format!("invalid JSON in '{}'", path.display()))?;
        let hooks = value
            .pointer("/hooks/PreToolUse")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("missing hooks.PreToolUse array"))?;
        let configured = hooks.iter().any(|entry| {
            entry
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(|command| command.contains(" hook copilot "))
        });
        if !configured {
            return Err(anyhow!(
                "Packet28 Copilot PreToolUse hook is not configured"
            ));
        }
        Ok(format!(
            "GitHub Copilot PreToolUse hook configured at {}",
            path.display()
        ))
    })();
    match result {
        Ok(detail) => DoctorCheck {
            name: "copilot_hook_config",
            ok: true,
            required: true,
            detail,
        },
        Err(err) => DoctorCheck {
            name: "copilot_hook_config",
            ok: false,
            required: true,
            detail: format!("copilot: {err}"),
        },
    }
}

fn check_cursor_hook_config(root: &Path) -> DoctorCheck {
    let path = cursor::hook_config_path(root);
    let result = (|| -> Result<String> {
        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read '{}'", path.display()))?;
        let value: Value = serde_json::from_str(&content)
            .with_context(|| format!("invalid JSON in '{}'", path.display()))?;
        let hooks = value
            .pointer("/hooks/beforeShellExecution")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("missing hooks.beforeShellExecution array"))?;
        let configured = hooks.iter().any(|entry| {
            entry
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(|command| command.contains(" hook cursor "))
        });
        if !configured {
            return Err(anyhow!(
                "Packet28 Cursor beforeShellExecution hook is not configured"
            ));
        }
        Ok(format!(
            "Cursor beforeShellExecution hook configured at {}",
            path.display()
        ))
    })();
    match result {
        Ok(detail) => DoctorCheck {
            name: "cursor_hook_config",
            ok: true,
            required: true,
            detail,
        },
        Err(err) => DoctorCheck {
            name: "cursor_hook_config",
            ok: false,
            required: true,
            detail: format!("cursor: {err}"),
        },
    }
}

fn build_gemini_report(root: &Path) -> DoctorReport {
    let daemon = DoctorCheck {
        name: "daemon",
        ok: true,
        required: false,
        detail: "not required for Gemini CLI hook verification".to_string(),
    };
    let index = DoctorCheck {
        name: "index",
        ok: true,
        required: false,
        detail: "not required for Gemini CLI hook verification".to_string(),
    };
    let mcp_config = Vec::new();
    let instruction_file =
        check_instruction_file("gemini", "Gemini CLI", &gemini::prompt_path(root));
    let hook_config = check_gemini_hook_config();
    let handshake = DoctorCheck {
        name: "mcp_handshake",
        ok: true,
        required: false,
        detail: "Gemini CLI uses a BeforeTool hook, not Packet28 MCP setup".to_string(),
    };
    let reducer_round_trip = DoctorCheck {
        name: "runtime_capture_support",
        ok: hook_config.ok,
        required: true,
        detail: hook_config.detail.clone(),
    };
    let push_notifications = DoctorCheck {
        name: "push_notifications",
        ok: true,
        required: false,
        detail: "not required for Gemini CLI hook verification".to_string(),
    };
    let handoff_round_trip = DoctorCheck {
        name: "handoff_round_trip",
        ok: true,
        required: false,
        detail: "not required for Gemini CLI hook verification".to_string(),
    };
    let checks = vec![
        daemon.clone(),
        index.clone(),
        instruction_file,
        hook_config,
        handshake.clone(),
        reducer_round_trip.clone(),
        push_notifications.clone(),
        handoff_round_trip.clone(),
    ];
    let ok = checks
        .iter()
        .filter(|check| check.required)
        .all(|check| check.ok);
    DoctorReport {
        root: root.display().to_string(),
        ok,
        daemon,
        index,
        mcp_config,
        handshake,
        reducer_round_trip,
        push_notifications,
        handoff_round_trip,
        checks,
    }
}

fn check_gemini_hook_config() -> DoctorCheck {
    let path = gemini::settings_path(&dirs_home());
    let result = (|| -> Result<String> {
        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read '{}'", path.display()))?;
        let value: Value = serde_json::from_str(&content)
            .with_context(|| format!("invalid JSON in '{}'", path.display()))?;
        let hooks = value
            .pointer("/hooks/BeforeTool")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("missing hooks.BeforeTool array"))?;
        let configured = hooks.iter().any(|entry| {
            entry.get("matcher").and_then(Value::as_str) == Some("run_shell_command")
                && entry
                    .get("hooks")
                    .and_then(Value::as_array)
                    .is_some_and(|commands| {
                        commands.iter().any(|command| {
                            command
                                .get("command")
                                .and_then(Value::as_str)
                                .is_some_and(|value| value.contains(" hook gemini "))
                        })
                    })
        });
        if !configured {
            return Err(anyhow!("Packet28 Gemini BeforeTool hook is not configured"));
        }
        Ok(format!(
            "Gemini CLI BeforeTool hook configured at {}",
            path.display()
        ))
    })();
    match result {
        Ok(detail) => DoctorCheck {
            name: "gemini_hook_config",
            ok: true,
            required: true,
            detail,
        },
        Err(err) => DoctorCheck {
            name: "gemini_hook_config",
            ok: false,
            required: true,
            detail: format!("gemini: {err}"),
        },
    }
}

fn build_opencode_report(root: &Path) -> DoctorReport {
    let daemon = DoctorCheck {
        name: "daemon",
        ok: true,
        required: false,
        detail: "not required for OpenCode plugin verification".to_string(),
    };
    let index = DoctorCheck {
        name: "index",
        ok: true,
        required: false,
        detail: "not required for OpenCode plugin verification".to_string(),
    };
    let mcp_config = Vec::new();
    let instruction_file =
        check_instruction_file("opencode", "OpenCode", &opencode::prompt_path(root));
    let plugin = check_opencode_plugin();
    let handshake = DoctorCheck {
        name: "mcp_handshake",
        ok: true,
        required: false,
        detail: "OpenCode uses a local TypeScript plugin, not Packet28 MCP setup".to_string(),
    };
    let reducer_round_trip = DoctorCheck {
        name: "runtime_command_preservation",
        ok: plugin.ok,
        required: true,
        detail: plugin.detail.clone(),
    };
    let push_notifications = DoctorCheck {
        name: "push_notifications",
        ok: true,
        required: false,
        detail: "not required for OpenCode plugin verification".to_string(),
    };
    let handoff_round_trip = DoctorCheck {
        name: "handoff_round_trip",
        ok: true,
        required: false,
        detail: "not required for OpenCode plugin verification".to_string(),
    };
    let checks = vec![
        daemon.clone(),
        index.clone(),
        instruction_file,
        plugin,
        handshake.clone(),
        reducer_round_trip.clone(),
        push_notifications.clone(),
        handoff_round_trip.clone(),
    ];
    let ok = checks
        .iter()
        .filter(|check| check.required)
        .all(|check| check.ok);
    DoctorReport {
        root: root.display().to_string(),
        ok,
        daemon,
        index,
        mcp_config,
        handshake,
        reducer_round_trip,
        push_notifications,
        handoff_round_trip,
        checks,
    }
}

fn check_opencode_plugin() -> DoctorCheck {
    let path = opencode::plugin_path(&dirs_home());
    let result = (|| -> Result<String> {
        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read '{}'", path.display()))?;
        if !content.contains("Packet28 preserves native command arguments")
            || !content.contains("tool.execute.before")
        {
            return Err(anyhow!(
                "Packet28 OpenCode command-preserving plugin is not configured"
            ));
        }
        Ok(format!(
            "OpenCode command-preserving plugin configured at {}",
            path.display()
        ))
    })();
    match result {
        Ok(detail) => DoctorCheck {
            name: "opencode_plugin",
            ok: true,
            required: true,
            detail,
        },
        Err(err) => DoctorCheck {
            name: "opencode_plugin",
            ok: false,
            required: true,
            detail: format!("opencode: {err}"),
        },
    }
}

fn build_hermes_report(root: &Path) -> DoctorReport {
    let daemon = DoctorCheck {
        name: "daemon",
        ok: true,
        required: false,
        detail: "not required for Hermes plugin verification".to_string(),
    };
    let index = DoctorCheck {
        name: "index",
        ok: true,
        required: false,
        detail: "not required for Hermes plugin verification".to_string(),
    };
    let mcp_config = Vec::new();
    let instruction_file = check_instruction_file("hermes", "Hermes", &hermes::prompt_path(root));
    let plugin = check_hermes_plugin();
    let handshake = DoctorCheck {
        name: "mcp_handshake",
        ok: true,
        required: false,
        detail: "Hermes uses a local Python plugin, not Packet28 MCP setup".to_string(),
    };
    let reducer_round_trip = DoctorCheck {
        name: "runtime_command_preservation",
        ok: plugin.ok,
        required: true,
        detail: plugin.detail.clone(),
    };
    let push_notifications = DoctorCheck {
        name: "push_notifications",
        ok: true,
        required: false,
        detail: "not required for Hermes plugin verification".to_string(),
    };
    let handoff_round_trip = DoctorCheck {
        name: "handoff_round_trip",
        ok: true,
        required: false,
        detail: "not required for Hermes plugin verification".to_string(),
    };
    let checks = vec![
        daemon.clone(),
        index.clone(),
        instruction_file,
        plugin,
        handshake.clone(),
        reducer_round_trip.clone(),
        push_notifications.clone(),
        handoff_round_trip.clone(),
    ];
    let ok = checks
        .iter()
        .filter(|check| check.required)
        .all(|check| check.ok);
    DoctorReport {
        root: root.display().to_string(),
        ok,
        daemon,
        index,
        mcp_config,
        handshake,
        reducer_round_trip,
        push_notifications,
        handoff_round_trip,
        checks,
    }
}

fn check_hermes_plugin() -> DoctorCheck {
    let home = dirs_home();
    check_hermes_plugin_at(&home)
}

fn check_hermes_plugin_at(home: &Path) -> DoctorCheck {
    let plugin_dir = hermes::plugin_dir(home);
    let init_path = plugin_dir.join("__init__.py");
    let manifest_path = plugin_dir.join("plugin.yaml");
    let config_path = hermes::config_path(home);
    let result = (|| -> Result<String> {
        let init = fs::read_to_string(&init_path)
            .with_context(|| format!("failed to read '{}'", init_path.display()))?;
        let manifest = fs::read_to_string(&manifest_path)
            .with_context(|| format!("failed to read '{}'", manifest_path.display()))?;
        let config = fs::read_to_string(&config_path)
            .with_context(|| format!("failed to read '{}'", config_path.display()))?;
        if !init.contains("Packet28 preserves native command arguments")
            || !manifest.contains("packet28-rewrite")
        {
            return Err(anyhow!("Packet28 Hermes plugin files are not configured"));
        }
        let enabled = crate::cmd_setup::setup_plugins::hermes_config_enables_packet28(&config)
            .with_context(|| format!("invalid YAML in '{}'", config_path.display()))?;
        if !enabled {
            return Err(anyhow!("Hermes config does not enable packet28-rewrite"));
        }
        Ok(format!(
            "Hermes command-preserving plugin configured at {}",
            plugin_dir.display()
        ))
    })();
    match result {
        Ok(detail) => DoctorCheck {
            name: "hermes_plugin",
            ok: true,
            required: true,
            detail,
        },
        Err(err) => DoctorCheck {
            name: "hermes_plugin",
            ok: false,
            required: true,
            detail: format!("hermes: {err}"),
        },
    }
}

fn build_windsurf_report(root: &Path) -> DoctorReport {
    let daemon = check_daemon(root);
    let index = check_index(root);
    let config_path = windsurf_mcp_config_path();
    let mcp_config = vec![inspect_mcp_config(&config_path)];
    let config_shape = check_windsurf_config_shape(root, &config_path);
    let rules = check_windsurf_rules(root);
    let handshake = check_windsurf_mcp_smoke();
    let reducer_round_trip = DoctorCheck {
        name: "windsurf_rewrite_support",
        ok: true,
        required: false,
        detail: "guidance-only: Packet28 does not claim Windsurf command interception without a real interception test".to_string(),
    };
    let push_notifications = DoctorCheck {
        name: "push_notifications",
        ok: true,
        required: false,
        detail: "not required for Windsurf MCP config verification".to_string(),
    };
    let handoff_round_trip = DoctorCheck {
        name: "handoff_round_trip",
        ok: true,
        required: false,
        detail: "not required for Windsurf MCP config verification".to_string(),
    };
    let checks = vec![
        daemon.clone(),
        index.clone(),
        summarize_mcp_config(root, &mcp_config),
        config_shape,
        rules,
        handshake.clone(),
        reducer_round_trip.clone(),
        push_notifications.clone(),
        handoff_round_trip.clone(),
    ];
    let ok = checks
        .iter()
        .filter(|check| check.required)
        .all(|check| check.ok);
    DoctorReport {
        root: root.display().to_string(),
        ok,
        daemon,
        index,
        mcp_config,
        handshake,
        reducer_round_trip,
        push_notifications,
        handoff_round_trip,
        checks,
    }
}

fn check_daemon(root: &Path) -> DoctorCheck {
    match crate::cmd_daemon::daemon_status_v1(root) {
        Ok(status) => {
            let cli_version = env!("PACKET28_VERSION");
            let daemon_version = status.version.trim();
            let version_ok = daemon_version == cli_version;
            DoctorCheck {
                name: "daemon",
                ok: version_ok,
                required: true,
                detail: if version_ok {
                    format!(
                        "daemon ready pid={} version={} socket={}",
                        status.pid, daemon_version, status.socket_path
                    )
                } else {
                    format!(
                        "daemon version skew: cli={} daemon={} pid={} socket={}; restart the daemon",
                        cli_version,
                        if daemon_version.is_empty() {
                            "<unknown>"
                        } else {
                            daemon_version
                        },
                        status.pid,
                        status.socket_path
                    )
                },
            }
        }
        Err(err) => DoctorCheck {
            name: "daemon",
            ok: false,
            required: true,
            detail: err.to_string(),
        },
    }
}

fn check_index(root: &Path) -> DoctorCheck {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match crate::cmd_daemon::send_request(
            root,
            &DaemonRequest::DaemonIndexStatus {
                request: DaemonIndexStatusRequest {
                    root: root.display().to_string(),
                },
            },
        ) {
            Ok(DaemonResponse::DaemonIndexStatus { response }) => {
                let ok = response.ready && response.manifest.status == DaemonIndexState::Ready;
                if ok || std::time::Instant::now() >= deadline {
                    return DoctorCheck {
                        name: "index",
                        ok,
                        required: true,
                        detail: format!(
                            "ready={} status={} generation={}",
                            response.ready, response.manifest.status, response.manifest.generation
                        ),
                    };
                }
            }
            Ok(other) => {
                return DoctorCheck {
                    name: "index",
                    ok: false,
                    required: true,
                    detail: format!("unexpected index status response: {other:?}"),
                };
            }
            Err(err) => {
                return DoctorCheck {
                    name: "index",
                    ok: false,
                    required: true,
                    detail: err.to_string(),
                };
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn collect_mcp_config_checks(root: &Path) -> Vec<McpConfigCheck> {
    let config_paths = [
        root.join(".mcp.json"),
        root.join(".cursor").join("mcp.json"),
    ];
    config_paths
        .into_iter()
        .map(|path| inspect_mcp_config(&path))
        .collect()
}

fn windsurf_mcp_config_path() -> std::path::PathBuf {
    windsurf::mcp_config_path(&dirs_home())
}

fn dirs_home() -> std::path::PathBuf {
    std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp"))
}

fn check_windsurf_config_shape(root: &Path, path: &Path) -> DoctorCheck {
    let result = (|| -> Result<String> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read '{}'", path.display()))?;
        let value: Value = serde_json::from_str(&content)
            .with_context(|| format!("invalid MCP config '{}'", path.display()))?;
        let server = value
            .get("mcpServers")
            .and_then(Value::as_object)
            .and_then(|servers| servers.get("packet28"))
            .and_then(Value::as_object)
            .ok_or_else(|| anyhow!("mcpServers.packet28 missing"))?;
        let command = server
            .get("command")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow!("mcpServers.packet28.command missing"))?;
        if !command_resolves(command) {
            return Err(anyhow!("MCP command '{command}' does not resolve"));
        }
        let args = server
            .get("args")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("mcpServers.packet28.args missing"))?;
        let arg_strings = args
            .iter()
            .map(|arg| {
                arg.as_str()
                    .map(ToOwned::to_owned)
                    .ok_or_else(|| anyhow!("mcpServers.packet28.args must be strings"))
            })
            .collect::<Result<Vec<_>>>()?;
        let expected_root = root.display().to_string();
        let root_arg_ok = arg_strings
            .windows(2)
            .any(|window| window[0] == "--root" && window[1] == expected_root);
        if !root_arg_ok {
            return Err(anyhow!(
                "mcpServers.packet28.args must preserve `--root {expected_root}`"
            ));
        }
        Ok(format!(
            "command resolves and args preserve repo root ({})",
            path.display()
        ))
    })();

    match result {
        Ok(detail) => DoctorCheck {
            name: "windsurf_config_shape",
            ok: true,
            required: true,
            detail,
        },
        Err(err) => DoctorCheck {
            name: "windsurf_config_shape",
            ok: false,
            required: true,
            detail: err.to_string(),
        },
    }
}

fn command_resolves(command: &str) -> bool {
    let path = Path::new(command);
    if path.components().count() > 1 || path.is_absolute() {
        return path.is_file();
    }
    std::process::Command::new("which")
        .arg(command)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn check_windsurf_rules(root: &Path) -> DoctorCheck {
    let path = windsurf::rule_path(root);
    let result = (|| -> Result<String> {
        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read '{}'", path.display()))?;
        if !content.contains("Windsurf hooks preserve native commands and permissions")
            || !content.contains("use explicit Packet28 CLI/MCP tools for reduced output")
        {
            return Err(anyhow!(
                "rules do not state native command preservation and explicit reduction guidance"
            ));
        }
        Ok(format!("rules present at {}", path.display()))
    })();
    match result {
        Ok(detail) => DoctorCheck {
            name: "windsurf_rules",
            ok: true,
            required: true,
            detail,
        },
        Err(err) => DoctorCheck {
            name: "windsurf_rules",
            ok: false,
            required: true,
            detail: err.to_string(),
        },
    }
}

fn check_windsurf_mcp_smoke() -> DoctorCheck {
    match crate::cmd_mcp::smoke_test_agent_config("windsurf") {
        Ok(report) => DoctorCheck {
            name: "windsurf_mcp_smoke",
            ok: true,
            required: true,
            detail: format!(
                "initialize/tools-list ok server={} tools={}",
                report.server_name, report.tool_count
            ),
        },
        Err(err) => DoctorCheck {
            name: "windsurf_mcp_smoke",
            ok: false,
            required: true,
            detail: err.to_string(),
        },
    }
}

fn inspect_mcp_config(path: &Path) -> McpConfigCheck {
    if !path.exists() {
        return McpConfigCheck {
            path: path.display().to_string(),
            exists: false,
            packet28_configured: false,
            detail: "file not found".to_string(),
        };
    }
    match mcp_config_has_packet28(path) {
        Ok(packet28_configured) => McpConfigCheck {
            path: path.display().to_string(),
            exists: true,
            packet28_configured,
            detail: if packet28_configured {
                "packet28 MCP entry present".to_string()
            } else {
                "packet28 MCP entry missing".to_string()
            },
        },
        Err(err) => McpConfigCheck {
            path: path.display().to_string(),
            exists: true,
            packet28_configured: false,
            detail: err.to_string(),
        },
    }
}

fn summarize_mcp_config(root: &Path, entries: &[McpConfigCheck]) -> DoctorCheck {
    let configured = entries
        .iter()
        .filter(|entry| entry.packet28_configured)
        .map(|entry| entry.path.clone())
        .collect::<Vec<_>>();
    let fallback_paths = ["AGENTS.md", "CLAUDE.md", ".cursorrules"]
        .into_iter()
        .map(|relative| root.join(relative))
        .filter(|path| path.exists())
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    let ok = !configured.is_empty();
    let detail = if ok {
        format!("configured MCP entries: {}", configured.join(", "))
    } else if !fallback_paths.is_empty() {
        format!(
            "no MCP config found; fallback guidance files present: {}",
            fallback_paths.join(", ")
        )
    } else {
        "no MCP config or fallback guidance files found".to_string()
    };
    DoctorCheck {
        name: "mcp_config",
        ok,
        required: false,
        detail,
    }
}

fn mcp_config_has_packet28(path: &Path) -> Result<bool> {
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read '{}'", path.display()))?;
    let value: Value = serde_json::from_str(&content)
        .with_context(|| format!("invalid MCP config '{}'", path.display()))?;
    Ok(value
        .get("mcpServers")
        .and_then(Value::as_object)
        .is_some_and(|servers| servers.contains_key("packet28")))
}

fn print_human_report(report: &DoctorReport) {
    println!("Packet28 doctor");
    println!("root: {}", report.root);
    for check in &report.checks {
        let status = if check.ok { "ok" } else { "fail" };
        let required = if check.required {
            "required"
        } else {
            "advisory"
        };
        println!("- {} [{}]: {}", check.name, required, status);
        println!("  {}", check.detail);
    }
    println!("overall: {}", if report.ok { "ok" } else { "fail" });
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::tempdir;

    fn write_hermes_fixture(home: &Path, config: &str) {
        let plugin_dir = hermes::plugin_dir(home);
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::write(
            plugin_dir.join("__init__.py"),
            "# Packet28 preserves native command arguments\n",
        )
        .unwrap();
        fs::write(plugin_dir.join("plugin.yaml"), "name: packet28-rewrite\n").unwrap();
        let config_path = hermes::config_path(home);
        fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        fs::write(config_path, config).unwrap();
    }

    #[test]
    fn windsurf_rules_accept_generated_capture_only_guidance() {
        let root = tempfile::tempdir().unwrap();
        let path = windsurf::rule_path(root.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let guidance = crate::agent_surface::render_prompt_fragment(
            crate::agent_surface::AgentPromptFormat::WindsurfRule,
            None,
        );
        fs::write(path, guidance).unwrap();
        assert!(check_windsurf_rules(root.path()).ok);
    }

    #[test]
    fn windsurf_rules_reject_legacy_rewrite_only_guidance() {
        let root = tempfile::tempdir().unwrap();
        let path = windsurf::rule_path(root.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "Windsurf command rewrite is not guaranteed\n").unwrap();
        assert!(!check_windsurf_rules(root.path()).ok);
    }

    #[test]
    fn claude_doctor_rejects_missing_hook_configuration() {
        let root = tempdir().unwrap();
        let settings = claude::settings_path(root.path());
        crate::cmd_setup::setup_hooks::write_claude_hook_config(&settings, root.path(), true)
            .unwrap();
        fs::write(&settings, "{}\n").unwrap();

        let check = check_claude_hook_config(root.path());

        assert!(!check.ok, "{}", check.detail);
        assert!(check.detail.contains("missing hooks.SessionStart"));
    }

    #[test]
    fn claude_doctor_accepts_generated_relocation_safe_hooks() {
        let root = tempdir().unwrap();
        let settings = claude::settings_path(root.path());
        crate::cmd_setup::setup_hooks::write_claude_hook_config(&settings, root.path(), true)
            .unwrap();

        let check = check_claude_hook_config(root.path());

        assert!(check.ok, "{}", check.detail);
    }

    #[test]
    fn hermes_doctor_accepts_tagged_unrelated_config() {
        let home = tempdir().unwrap();
        write_hermes_fixture(
            home.path(),
            r#"
workspace: !Packet28
  id: !!str 001
plugins:
  enabled:
    - packet28-rewrite
"#,
        );

        let check = check_hermes_plugin_at(home.path());

        assert!(check.ok, "{}", check.detail);
    }

    #[test]
    fn hermes_doctor_reports_malformed_yaml() {
        let home = tempdir().unwrap();
        write_hermes_fixture(home.path(), "plugins: [");

        let check = check_hermes_plugin_at(home.path());

        assert!(
            !check.ok && check.detail.contains("invalid YAML"),
            "{}",
            check.detail
        );
    }
}
