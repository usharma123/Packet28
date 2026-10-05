use std::path::Path;

use packet28_daemon_protocol::hooks::{
    is_legacy_generated_relaunch_command, HookRuntimeConfig, RelaunchPreference,
};

/// Setup never enables daemon-managed relaunch on its own any more.
///
/// Earlier releases wrote `relaunch_preference = daemon_managed` with a
/// generated `claude --continue` (or `packet28-agent`) command whenever
/// `packet28-agent` was on PATH. That made every `Stop`/`SubagentStop` boundary
/// spawn a headless `claude --continue` that resumed the user's live session
/// and multiplied processes on multi-agent runs. This helper migrates those
/// stale configs back to host-managed and leaves user-authored commands alone.
pub(super) fn apply_generated_relaunch_command(config: &mut HookRuntimeConfig) -> bool {
    let managed_by_setup = config.relaunch_command.is_empty()
        || is_legacy_generated_relaunch_command(&config.relaunch_command);
    if !managed_by_setup {
        return false;
    }
    if config.relaunch_preference == RelaunchPreference::HostManaged
        && config.relaunch_command.is_empty()
    {
        return false;
    }
    config.relaunch_preference = RelaunchPreference::HostManaged;
    config.relaunch_command.clear();
    true
}

pub(super) fn shell_escape(value: String) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`")
}

pub(crate) fn generated_packet28_hook_command(runtime: &str, root: &Path) -> String {
    let command = resolve_packet28_cli_command();
    if runtime == "claude" {
        let command_arg = shell_escape(command);
        return guarded_packet28_hook_command_with_root_arg(
            &command_arg,
            runtime,
            "${CLAUDE_PROJECT_DIR}",
        );
    }
    guarded_packet28_hook_command(&command, runtime, root)
}

/// Recognizes only a complete command serialized by the setup generator.
///
/// `shell_words` recovers argument values but does not parse shell operators.
/// Reconstructing the complete command rejects appended user shell syntax.
pub(crate) fn is_generated_packet28_hook_command(command: &str, runtime: &str) -> bool {
    let Ok(argv) = shell_words::split(command) else {
        return false;
    };
    if argv.len() != 6 {
        return false;
    }
    if command == guarded_packet28_hook_command(&argv[4], runtime, Path::new(&argv[5])) {
        return true;
    }
    runtime == "claude"
        && argv[5] == "${CLAUDE_PROJECT_DIR}"
        && command
            == guarded_packet28_hook_command_with_root_arg(
                &shell_escape(argv[4].clone()),
                runtime,
                "${CLAUDE_PROJECT_DIR}",
            )
}

pub(crate) fn guarded_packet28_hook_command(
    packet28_command: &str,
    runtime: &str,
    root: &Path,
) -> String {
    let command_arg = shell_escape(packet28_command.to_string());
    let root_arg = shell_escape(root.display().to_string());
    guarded_packet28_hook_command_with_root_arg(&command_arg, runtime, &root_arg)
}

fn guarded_packet28_hook_command_with_root_arg(
    packet28_command: &str,
    runtime: &str,
    root_arg: &str,
) -> String {
    format!(
        "sh -c 'if [ -x \"$1\" ] || command -v \"$1\" >/dev/null 2>&1; then exec \"$1\" hook {runtime} --root \"$2\"; fi; exit 0' packet28-hook \"{packet28_command}\" \"{root_arg}\""
    )
}

pub(crate) fn resolve_packet28_mcp_command() -> String {
    let output = std::process::Command::new("which")
        .arg("packet28-mcp")
        .output();
    if let Ok(output) = output {
        if output.status.success() {
            let command = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !command.is_empty() {
                return command;
            }
        }
    }
    "packet28-mcp".to_string()
}

pub(super) fn resolve_packet28_cli_command() -> String {
    for candidate in ["Packet28", "packet28"] {
        let output = std::process::Command::new("which").arg(candidate).output();
        if let Ok(output) = output {
            if output.status.success() {
                let command = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !command.is_empty() {
                    return command;
                }
            }
        }
    }
    "Packet28".to_string()
}
