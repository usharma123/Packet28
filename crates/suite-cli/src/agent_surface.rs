use std::path::{Path, PathBuf};

use clap::ValueEnum;

use crate::cmd_common::JsonProfileArg;

pub const DEFAULT_PREFLIGHT_PROFILE: JsonProfileArg = JsonProfileArg::Compact;
pub const LATEST_PREFLIGHT_RELATIVE_PATH: &str = ".packet28/agent/latest-preflight.json";
const TASK_PLACEHOLDER: &str = "<natural-language task>";
const ROOT_PLACEHOLDER: &str = "<path>";

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AgentPromptFormat {
    Claude,
    Agents,
    Cursor,
}

pub fn latest_preflight_path(root: &Path) -> PathBuf {
    root.join(LATEST_PREFLIGHT_RELATIVE_PATH)
}

pub fn preflight_command_example(root: Option<&str>) -> String {
    format!(
        "Packet28 preflight{} --task \"{}\" --json=compact",
        command_root_fragment(root),
        TASK_PLACEHOLDER
    )
}

pub fn mcp_command_example(root: Option<&str>) -> String {
    format!("Packet28 mcp serve{}", command_root_fragment(root),)
}

pub fn wrapper_command_example() -> &'static str {
    "packet28-agent --task \"<natural-language task>\" -- <agent command...>"
}

pub fn render_prompt_fragment(format: AgentPromptFormat, root: Option<&str>) -> String {
    let preflight = preflight_command_example(root);
    let mcp = mcp_command_example(root);
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
- Prefer `{mcp}` and maintain a local `sections_by_id` cache keyed by `task_id` and `context_version`.\n\
- Treat the latest Packet28 brief as the only canonical Packet28 context block; replace older Packet28 blocks instead of appending them.\n\
- Call `packet28.estimate_context` before cheap or budget-constrained actions to decide whether a full fetch is worth it.\n\
- For constrained refactors, `packet28.decompose` is available as an experimental helper; refine the returned steps locally, then run `packet28.validate_plan` before execution.\n\
- Call `packet28.get_context` before each substantive model/tool invocation using `since_version` and `response_mode=\"auto\"`.\n\
- Patch the local section cache from `delta.changed_sections` and `delta.removed_section_ids` instead of replacing everything.\n\
- Respect the supersession header in each brief and use it to ignore older Packet28 context.\n\
- Use explicit section filters and section-item limits before falling back to deprecated `verbosity`.\n\
- After file reads, edits, checkpoints, decisions, and question updates, call `packet28.write_state`.\n\
- Watch for `notifications/packet28.context_updated`; if notifications are unavailable, poll with `since_version` on the next invocation.\n\
- Use `packet28://task/<task_id>/brief` or `.packet28/task/<task_id>/brief.md` only as a fallback bridge when MCP is unavailable.\n\
- `{preflight}` remains a compatibility path for one-shot startup context.\n\
- If Packet28 is unavailable or returns insufficient context, fall back to direct file reads and commands.\n\
- Do not require preflight for trivial conversational requests or narrow single-file edits.\n\
- {root_note}\n"
        ),
        AgentPromptFormat::Agents => format!(
            "## Packet28 Guidance\n\
When the task is substantial, use Packet28 as a live broker rather than a one-shot preflight only.\n\
\n\
- MCP command: `{mcp}`\n\
- Keep a local `sections_by_id` cache keyed by `task_id` and `context_version`.\n\
- Replace the prior Packet28 context block each turn instead of appending historical Packet28 briefs.\n\
- Call `packet28.estimate_context` before cheap actions or when near budget.\n\
- Use the experimental `packet28.decompose` helper for constrained refactors, refine the returned steps, then call `packet28.validate_plan` before execution.\n\
- Call `packet28.get_context` before planning, inspection, tool choice, interpretation, edits, and summaries using `since_version` and `response_mode=\"auto\"`.\n\
- Patch the local section cache from `delta.changed_sections` and `delta.removed_section_ids`.\n\
- Respect the supersession header in each brief and keep one mutable Packet28 block in the runtime prompt.\n\
- Prefer explicit section filters and section-item limits; treat `verbosity` as compatibility-only.\n\
- Call `packet28.write_state` after file reads, edits, checkpoints, decisions, and question updates.\n\
- Watch for `notifications/packet28.context_updated` and fall back to polling `since_version` when notifications are unavailable.\n\
- Use the task brief file/resource only as a compatibility fallback.\n\
- Fall back to direct file reads if Packet28 is unavailable, errors, or does not provide enough context.\n\
- Skip mandatory preflight for trivial chat or isolated single-file edits.\n\
- {root_note}\n"
        ),
        AgentPromptFormat::Cursor => format!(
            "Packet28 integration:\n\
- Start `{mcp}` and keep a local `sections_by_id` cache keyed by `task_id` and `context_version`.\n\
- Keep one mutable Packet28 context block and replace it whenever a newer brief supersedes the old one.\n\
- Call `packet28.estimate_context` before cheap actions or when near budget.\n\
- For constrained refactors, use the experimental `packet28.decompose` helper, refine the returned steps, then call `packet28.validate_plan` before execution.\n\
- Call `packet28.get_context` before each substantive invocation using `since_version` and `response_mode=\"auto\"`.\n\
- Patch the local section cache from `delta.changed_sections` and `delta.removed_section_ids`.\n\
- Respect the supersession header in each brief and use it to discard older Packet28 reasoning context.\n\
- Prefer explicit section filters and section-item limits; use `verbosity` only as a compatibility alias.\n\
- Call `packet28.write_state` after file reads, edits, checkpoints, decisions, and question updates.\n\
- Watch for `notifications/packet28.context_updated`; if notifications are unavailable, poll with `since_version`.\n\
- Use `.packet28/task/<task_id>/brief.md` only as a fallback bridge when MCP is unavailable.\n\
- `{preflight}` remains available for compatibility startup context.\n\
- If Packet28 is unavailable, fails, or returns insufficient context, fall back to direct file reads and commands.\n\
- Do not force preflight for trivial chat or isolated single-file edits.\n\
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
    fn preflight_example_uses_requested_root() {
        let rendered = preflight_command_example(Some("repo"));
        assert!(rendered.contains("--root \"repo\""));
        assert!(rendered.contains("--json=compact"));
    }

    #[test]
    fn claude_fragment_contains_required_guidance() {
        let rendered = render_prompt_fragment(AgentPromptFormat::Claude, None);
        assert!(rendered.contains("Packet28 as a live context broker"));
        assert!(rendered.contains("packet28.get_context"));
        assert!(rendered.contains("packet28.estimate_context"));
        assert!(rendered.contains("packet28.write_state"));
        assert!(rendered.contains("sections_by_id"));
        assert!(rendered.contains("packet28.validate_plan"));
        assert!(rendered.contains("packet28.decompose"));
        assert!(rendered.contains("fall back to direct file reads and commands"));
        assert!(rendered.contains("brief.md"));
    }

    #[test]
    fn cursor_fragment_mentions_non_trivial_scope() {
        let rendered = render_prompt_fragment(AgentPromptFormat::Cursor, None);
        assert!(rendered.contains("packet28.get_context"));
        assert!(rendered.contains("Packet28 mcp serve"));
        assert!(rendered.contains("sections_by_id"));
        assert!(rendered.contains("packet28.validate_plan"));
        assert!(rendered.contains("single-file edits"));
    }
}
