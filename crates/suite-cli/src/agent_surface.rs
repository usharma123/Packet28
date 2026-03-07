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

pub fn wrapper_command_example() -> &'static str {
    "packet28-agent --task \"<natural-language task>\" -- <agent command...>"
}

pub fn render_prompt_fragment(format: AgentPromptFormat, root: Option<&str>) -> String {
    let preflight = preflight_command_example(root);
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
Use Packet28 preflight before broad raw-file reading for non-trivial coding, debugging, test, review, or refactor tasks.\n\
\n\
- Run `{preflight}` before scanning large parts of the repo.\n\
- Prefer the returned preflight packets over broad raw-file reads.\n\
- If preflight is unavailable, fails, or returns insufficient context, fall back to direct file reads and commands.\n\
- Use daemon task/watch flows only for long-running tasks that need refreshed context after repo changes; do not use them for one-shot work.\n\
- Do not require preflight for trivial conversational requests or narrow single-file edits.\n\
- {root_note}\n"
        ),
        AgentPromptFormat::Agents => format!(
            "## Packet28 Guidance\n\
When the task is substantial, start with Packet28 preflight before broad repository exploration.\n\
\n\
- Command: `{preflight}`\n\
- Prefer bounded preflight packets over large raw file sweeps.\n\
- Fall back to direct file reads if preflight is unavailable, errors, or does not provide enough context.\n\
- Reserve daemon task/watch usage for long-running tasks that need passive refresh on file changes.\n\
- Skip mandatory preflight for trivial chat or isolated single-file edits.\n\
- {root_note}\n"
        ),
        AgentPromptFormat::Cursor => format!(
            "Packet28 integration:\n\
- Before broad raw-file reading for non-trivial coding, debugging, test, review, or refactor tasks, run `{preflight}`.\n\
- Prefer the returned preflight packets over large raw-file scans.\n\
- If preflight is unavailable, fails, or returns insufficient context, fall back to direct file reads and commands.\n\
- Use daemon task/watch features only for long-running tasks that need refreshed context after changes.\n\
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
        assert!(rendered.contains("Packet28 preflight"));
        assert!(rendered.contains("Prefer the returned preflight packets"));
        assert!(rendered.contains("fall back to direct file reads and commands"));
        assert!(rendered.contains("daemon task/watch"));
    }

    #[test]
    fn cursor_fragment_mentions_non_trivial_scope() {
        let rendered = render_prompt_fragment(AgentPromptFormat::Cursor, None);
        assert!(rendered.contains("non-trivial coding, debugging, test, review, or refactor tasks"));
        assert!(rendered.contains("single-file edits"));
    }
}
