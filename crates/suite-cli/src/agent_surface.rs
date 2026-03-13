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
Use Packet28 as a live context broker for non-trivial coding, debugging, test, review, refactor, or design tasks.\n\
\n\
- Prefer `{mcp}` and treat Packet28 as a reducer plus handoff broker, not a thick mid-turn context fetcher.\n\
- When you can put Packet28 in front of your MCP tools, prefer `{proxy}` so tool activity is auto-captured.\n\
- Use slim in-turn reducers such as `packet28.search` and `packet28.read_regions`; fetch full artifacts only on demand.\n\
- Persist reads, edits, checkpoints, and worker intent with `packet28.write_state`.\n\
- For long-running work, write the latest intention, save a checkpoint, assemble handoff with `packet28.prepare_handoff`, then relaunch a fresh worker.\n\
- Treat the latest Packet28 brief as the only canonical Packet28 context block; replace older Packet28 blocks instead of appending them.\n\
- Use `packet28.fetch_context` only for explicit inspection or debugging of an assembled handoff/context artifact.\n\
- Respect the supersession header in each brief and use it to ignore older Packet28 context.\n\
- Use explicit section filters and section-item limits before falling back to deprecated `verbosity`.\n\
- The daemon or wrapper should own relaunch timing; do not grow one worker session indefinitely.\n\
- Use `packet28://task/<task_id>/brief` or `.packet28/task/<task_id>/brief.md` only as a fallback bridge when MCP is unavailable.\n\
- If Packet28 is unavailable or returns insufficient context, fall back to direct file reads and commands.\n\
- Do not force handoff/bootstrap orchestration for trivial conversational requests or narrow single-file edits.\n\
- {root_note}\n"
        ),
        AgentPromptFormat::Agents => format!(
            "## Packet28 Guidance\n\
When the task is substantial, use Packet28 as a live broker with slim reducers in-turn and checkpointed handoff between workers.\n\
\n\
- MCP command: `{mcp}`\n\
- Preferred MCP endpoint when available: `{proxy}`\n\
- Use slim in-turn reducers such as `packet28.search` and `packet28.read_regions`; fetch full artifacts only when needed.\n\
- For long-running work, persist worker intent with `packet28.write_state(op=\"intention\", ...)`, checkpoint explicitly, then use `packet28.prepare_handoff` to bootstrap a fresh worker.\n\
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
- Start `{mcp}` and use Packet28 as a reducer plus handoff broker.\n\
- Prefer `{proxy}` when you want Packet28 to auto-capture tool activity.\n\
- Prefer slim in-turn reducers such as `packet28.search` and `packet28.read_regions`, plus explicit artifact fetches for detail.\n\
- For checkpointed relaunch flows, write the latest worker intention and use `packet28.prepare_handoff` to seed the next worker.\n\
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
        assert!(rendered.contains("Packet28 as a live context broker"));
        assert!(rendered.contains("packet28.search"));
        assert!(rendered.contains("packet28.read_regions"));
        assert!(rendered.contains("packet28.write_state"));
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
