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
}

pub fn latest_bootstrap_path(root: &Path) -> PathBuf {
    root.join(LATEST_BOOTSTRAP_RELATIVE_PATH)
}

pub fn latest_handoff_path(root: &Path) -> PathBuf {
    root.join(LATEST_HANDOFF_RELATIVE_PATH)
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
    let root_note = if root.is_some() {
        format!(
            "Use `--root {}` only when the agent is operating outside the repository root.",
            ROOT_PLACEHOLDER
        )
    } else {
        "Use `--root <path>` only when the agent is operating outside the repository root."
            .to_string()
    };

    match format {
        AgentPromptFormat::Claude => format!(
            "## Packet28\n\
Use Packet28 as a hooks-first reducer runtime for non-trivial coding, debugging, test, review, refactor, or design tasks.\n\
\n\
- Start with `{mcp}` for Packet28 control-plane tools and install Claude hooks with `Packet28 setup`.\n\
- Let Claude hooks rewrite supported Bash commands through Packet28 reducers and capture native tool activity automatically; do not call reducer MCP tools in the active loop.\n\
- Use `packet28.write_intention` only when the task objective or next step changes materially.\n\
- Let the daemon assemble handoff context after threshold or stop boundaries; do not grow one worker session indefinitely.\n\
- Use `packet28.prepare_handoff` and `packet28.fetch_context` only for explicit handoff/bootstrap or inspection flows.\n\
- Treat the latest Packet28 brief as the only canonical Packet28 context block; replace older Packet28 blocks instead of appending them.\n\
- Respect the supersession header in each brief and use it to ignore older Packet28 context.\n\
- Use explicit section filters and section-item limits before falling back to deprecated `verbosity`.\n\
- Use `packet28://task/<task_id>/brief` or `.packet28/task/<task_id>/brief.md` only as a fallback bridge when MCP is unavailable.\n\
- If Packet28 is unavailable or returns insufficient context, fall back to direct file reads and commands.\n\
- Do not force handoff/bootstrap orchestration for trivial conversational requests or narrow single-file edits.\n\
- {root_note}\n"
        ),
        AgentPromptFormat::Agents => format!(
            "## Packet28 Guidance\n\
When the task is substantial, use Packet28 as a hooks-first reducer runtime with checkpointed handoff between workers.\n\
\n\
- MCP command: `{mcp}`\n\
- Preferred MCP endpoint when available: `{proxy}`\n\
- Claude hooks, not MCP reducer tools, should rewrite supported shell commands and capture routine tool activity into Packet28.\n\
- Use `packet28.write_intention` only for semantic task intent; avoid repeated generic state writes in the loop.\n\
- Let the daemon prepare handoff context after threshold or stop boundaries, then resume from the latest handoff packet.\n\
- Replace the prior Packet28 context block each turn instead of appending historical Packet28 briefs.\n\
- Keep thick context assembly out of the active worker loop; use `packet28.fetch_context` only for explicit artifact inspection.\n\
- The daemon or wrapper should own fresh-worker relaunch after checkpointed handoff assembly.\n\
- Respect the supersession header in each brief and keep one mutable Packet28 block in the runtime prompt.\n\
- Prefer explicit section filters and section-item limits; treat `verbosity` as compatibility-only.\n\
- Use the task brief file/resource only as a compatibility fallback.\n\
- Fall back to direct file reads if Packet28 is unavailable, errors, or does not provide enough context.\n\
- Skip handoff/bootstrap ceremony for trivial chat or isolated single-file edits.\n\
- {root_note}\n"
        ),
        AgentPromptFormat::Cursor => format!(
            "Packet28 integration:\n\
- Start `{mcp}` and use Packet28 as a control-plane plus handoff broker.\n\
- Prefer `{proxy}` when you want Packet28 to auto-capture upstream tool activity.\n\
- Use `packet28.write_intention` for semantic objective updates and keep rewrite/capture out of the visible MCP loop.\n\
- For checkpointed relaunch flows, use `packet28.prepare_handoff` to seed the next worker.\n\
- Keep one mutable Packet28 context block and replace it whenever a newer brief supersedes the old one.\n\
- Use `packet28.fetch_context` only when you explicitly need to inspect a stored handoff or context artifact.\n\
- Prefer relaunching a fresh worker after checkpointed handoff assembly instead of keeping one session hot.\n\
- Respect the supersession header in each brief and use it to discard older Packet28 reasoning context.\n\
- Prefer explicit section filters and section-item limits; use `verbosity` only as a compatibility alias.\n\
- Use `.packet28/task/<task_id>/brief.md` only as a fallback bridge when MCP is unavailable.\n\
- If Packet28 is unavailable, fails, or returns insufficient context, fall back to direct file reads and commands.\n\
- Do not force handoff/bootstrap orchestration for trivial chat or isolated single-file edits.\n\
- {root_note}\n"
        ),
    }
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
        assert!(rendered.contains("hooks-first reducer runtime"));
        assert!(rendered.contains("packet28.write_intention"));
        assert!(rendered.contains("packet28.prepare_handoff"));
        assert!(rendered.contains("fall back to direct file reads and commands"));
        assert!(rendered.contains("brief.md"));
    }

    #[test]
    fn cursor_fragment_mentions_non_trivial_scope() {
        let rendered = render_prompt_fragment(AgentPromptFormat::Cursor, None);
        assert!(rendered.contains("packet28.prepare_handoff"));
        assert!(rendered.contains("packet28.fetch_context"));
        assert!(rendered.contains("Packet28 mcp serve"));
        assert!(rendered.contains("single-file edits"));
    }
}
