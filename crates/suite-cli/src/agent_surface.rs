use std::path::{Path, PathBuf};

use clap::ValueEnum;
pub const LATEST_BOOTSTRAP_RELATIVE_PATH: &str = ".packet28/agent/latest-bootstrap.json";
pub const LATEST_HANDOFF_RELATIVE_PATH: &str = ".packet28/agent/latest-handoff.json";
const ROOT_PLACEHOLDER: &str = "<path>";

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AgentPromptFormat {
    Claude,
    Agents,
    Cursor,
    CursorRule,
    WindsurfRule,
}

pub fn latest_bootstrap_path(root: &Path) -> PathBuf {
    root.join(LATEST_BOOTSTRAP_RELATIVE_PATH)
}

pub fn latest_handoff_path(root: &Path) -> PathBuf {
    root.join(LATEST_HANDOFF_RELATIVE_PATH)
}

pub(crate) fn contains_packet28_guidance(content: &str) -> bool {
    content.contains("packet28.write_intention")
        || content.contains("packet28.prepare_handoff")
        || content.contains("Packet28 mcp serve")
        || content.contains("hook claude")
}

pub fn mcp_command_example(root: Option<&str>) -> String {
    format!("Packet28 mcp serve{}", command_root_fragment(root),)
}

pub fn mcp_proxy_command_example(root: Option<&str>) -> String {
    format!(
        "Packet28 mcp proxy{} --upstream-config .mcp.proxy.json",
        command_root_fragment(root),
    )
}

pub fn wrapper_command_example() -> &'static str {
    "packet28-agent --task-id <task-id> --wait-for-handoff -- <agent command...>"
}

pub fn render_prompt_fragment(format: AgentPromptFormat, root: Option<&str>) -> String {
    let mcp = mcp_command_example(root);
    let proxy = mcp_proxy_command_example(root);
    let wrapper = wrapper_command_example();
    let (header, runtime_note, tool_prefix) = match format {
        AgentPromptFormat::Claude => (
            "## Packet28\n",
            "Claude hooks installed by `Packet28 setup` capture tool activity without changing commands or native permission matching. Use explicit Packet28 CLI/MCP tools for reduced output.\n",
            "packet28.",
        ),
        AgentPromptFormat::Agents => (
            "## Packet28 Guidance\n",
            "Runtime integrations preserve native command arguments and permission matching. Hooks capture tool activity where supported; use explicit Packet28 CLI/MCP tools for reduced output.\n",
            "packet28.",
        ),
        AgentPromptFormat::Cursor | AgentPromptFormat::CursorRule => (
            if format == AgentPromptFormat::CursorRule {
                "---\ndescription: Packet28 runtime guidance\nglobs:\nalwaysApply: true\n---\n\n# Packet28 Integration\n"
            } else {
                "Packet28 integration:\n"
            },
            "Cursor uses `packet28_*` tool names. Dotted aliases such as `packet28.write_intention` and `packet28.read_regions` work where supported.\n",
            "packet28_",
        ),
        AgentPromptFormat::WindsurfRule => (
            "---\ndescription: Packet28 runtime guidance\ntrigger: always_on\n---\n\n# Packet28 Integration\n",
            "Windsurf hooks preserve native commands and permissions; use explicit Packet28 CLI/MCP tools for reduced output. Check runtime installation with `Packet28 doctor --agent windsurf`.\n",
            "packet28.",
        ),
    };

    format!(
        "{header}\n\
Use Packet28 for substantial work; skip handoff setup for trivial chat or isolated single-file edits.\n\
{runtime_note}\n\
- Start `{mcp}`; use `{proxy}` to capture upstream MCP activity.\n\
- Prefer `p28` instant grep when available; use `{tool_prefix}read_regions` and `{tool_prefix}glob` for compact reads, and `{tool_prefix}fetch_tool_result` for the full stored artifact.\n\
- Call `{tool_prefix}write_intention` when the objective, decision, or next step changes; `{tool_prefix}task_status` only for readiness or artifact IDs. Let hooks handle routine capture.\n\
- Use `{tool_prefix}prepare_handoff` at checkpoints and `{tool_prefix}fetch_context` for resume or explicit artifact inspection. Let the daemon or `{wrapper}` resume a fresh worker.\n\
- Keep repository instructions stable. Use the newest brief's supersession header to resolve stale task context. Put brief updates after stable instructions; do not rewrite earlier conversation messages. At compaction or fresh-worker resume, include only the latest brief.\n\
- If MCP is unavailable, read `packet28://task/<task_id>/brief` or `.packet28/task/<task_id>/brief.md`. If Packet28 fails or lacks context, fall back to direct file reads and commands.\n\
- Use `--root {ROOT_PLACEHOLDER}` only outside the repository root.\n"
    )
}

fn command_root_fragment(root: Option<&str>) -> String {
    match root {
        Some(root) if !root.trim().is_empty() && root.trim() != "." => {
            format!(" --root \"{}\"", root.trim())
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_example_uses_requested_root() {
        let rendered = mcp_command_example(Some("repo"));
        assert!(rendered.contains("--root \"repo\""));
    }

    #[test]
    fn claude_fragment_contains_required_guidance() {
        let rendered = render_prompt_fragment(AgentPromptFormat::Claude, None);
        assert!(rendered.contains("Use Packet28 for substantial work"));
        assert!(rendered.contains("p28` instant grep"));
        assert!(rendered.contains("packet28.read_regions"));
        assert!(rendered.contains("packet28.write_intention"));
        assert!(rendered.contains("packet28.prepare_handoff"));
        assert!(rendered.contains("packet28.task_status"));
        assert!(rendered.contains("packet28.fetch_tool_result"));
        assert!(rendered.contains("fall back to direct file reads and commands"));
        assert!(rendered.contains("brief.md"));
        assert!(!rendered.contains("packet28.search"));
    }

    #[test]
    fn agents_fragment_tracks_current_workflow() {
        let rendered = render_prompt_fragment(AgentPromptFormat::Agents, None);
        assert!(rendered.contains("Use Packet28 for substantial work"));
        assert!(rendered.contains("p28` instant grep"));
        assert!(rendered.contains("packet28.read_regions"));
        assert!(rendered.contains("packet28.fetch_tool_result"));
        assert!(rendered.contains("packet28.task_status"));
        assert!(rendered.contains("packet28.prepare_handoff"));
        assert!(rendered.contains("write_intention"));
        assert!(rendered.contains("packet28-agent --task-id <task-id>"));
        assert!(!rendered.contains("write_state"));
        assert!(!rendered.contains("get_context"));
        assert!(!rendered.contains("packet28.search"));
    }

    #[test]
    fn cursor_fragment_mentions_non_trivial_scope() {
        let rendered = render_prompt_fragment(AgentPromptFormat::Cursor, None);
        assert!(rendered.contains("packet28.write_intention"));
        assert!(rendered.contains("packet28.read_regions"));
        assert!(rendered.contains("packet28_prepare_handoff"));
        assert!(rendered.contains("packet28_fetch_context"));
        assert!(rendered.contains("packet28_glob"));
        assert!(rendered.contains("packet28_fetch_tool_result"));
        assert!(rendered.contains("packet28_task_status"));
        assert!(rendered.contains("Packet28 mcp serve"));
        assert!(rendered.contains("single-file edits"));
    }

    #[test]
    fn cursor_rule_fragment_has_frontmatter() {
        let rendered = render_prompt_fragment(AgentPromptFormat::CursorRule, None);
        assert!(rendered.starts_with("---\n"));
        assert!(rendered.contains("alwaysApply: true"));
        assert!(rendered.contains("# Packet28 Integration"));
    }

    #[test]
    fn windsurf_rule_fragment_has_trigger_frontmatter() {
        let rendered = render_prompt_fragment(AgentPromptFormat::WindsurfRule, None);
        assert!(rendered.starts_with("---\n"));
        assert!(rendered.contains("trigger: always_on"));
        assert!(rendered.contains("# Packet28 Integration"));
    }
    #[test]
    fn fragments_keep_stable_instructions_separate_from_brief_updates() {
        for format in [
            AgentPromptFormat::Claude,
            AgentPromptFormat::Agents,
            AgentPromptFormat::Cursor,
            AgentPromptFormat::CursorRule,
            AgentPromptFormat::WindsurfRule,
        ] {
            let rendered = render_prompt_fragment(format, None);
            assert!(rendered.contains("do not rewrite earlier conversation messages"));
            assert!(rendered
                .contains("At compaction or fresh-worker resume, include only the latest brief"));
            assert_eq!(rendered.matches("supersession").count(), 1);
            let prefix = if matches!(
                format,
                AgentPromptFormat::Cursor | AgentPromptFormat::CursorRule
            ) {
                "packet28_"
            } else {
                "packet28."
            };
            for tool in [
                "read_regions",
                "glob",
                "fetch_tool_result",
                "write_intention",
                "task_status",
                "prepare_handoff",
                "fetch_context",
            ] {
                assert_eq!(rendered.matches(&format!("`{prefix}{tool}`")).count(), 1);
            }
        }
    }

    #[test]
    fn hook_guidance_preserves_native_commands_and_explicit_reduction() {
        let claude = render_prompt_fragment(AgentPromptFormat::Claude, None);
        assert!(claude.contains("hooks installed by `Packet28 setup` capture tool activity"));
        assert!(claude.contains("without changing commands or native permission matching"));
        let agents = render_prompt_fragment(AgentPromptFormat::Agents, None);
        assert!(agents.contains("preserve native command arguments and permission matching"));
        for rendered in [claude, agents] {
            assert!(rendered.contains("explicit Packet28 CLI/MCP tools for reduced output"));
        }
        for format in [AgentPromptFormat::Cursor, AgentPromptFormat::CursorRule] {
            assert!(!render_prompt_fragment(format, None).contains("`Packet28 setup`"));
        }
        let windsurf = render_prompt_fragment(AgentPromptFormat::WindsurfRule, None);
        assert!(windsurf.contains("Windsurf hooks preserve native commands and permissions"));
        assert!(windsurf.contains("explicit Packet28 CLI/MCP tools for reduced output"));
        assert!(windsurf.contains("Packet28 doctor --agent windsurf"));
    }
}
