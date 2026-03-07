use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser};

#[derive(Debug, Parser)]
#[command(
    name = "packet28-agent",
    version,
    about = "Run Packet28 preflight before delegating to an agent runtime",
    trailing_var_arg = true,
    after_help = "Example:\n  packet28-agent --task \"investigate flaky parser test\" -- codex exec \"review the failure\""
)]
pub struct Packet28AgentCli {
    /// Natural-language task description to send to Packet28 preflight
    #[arg(long)]
    pub task: String,

    /// Root path for repo-aware preflight reducers
    #[arg(long, default_value = ".")]
    pub root: String,

    /// Optional task identifier for recall scoping
    #[arg(long)]
    pub task_id: Option<String>,

    /// Preflight JSON profile to persist for the delegated agent
    #[arg(long, value_enum, default_value_t = crate::agent_surface::DEFAULT_PREFLIGHT_PROFILE)]
    pub json: crate::cmd_common::JsonProfileArg,

    /// Delegated agent command. Pass it after `--` so wrapper flags do not leak into the child.
    #[arg(allow_hyphen_values = true)]
    pub command: Vec<String>,
}

pub fn main_entry() {
    let cli = Packet28AgentCli::parse();
    if cli.command.is_empty() {
        let mut command = Packet28AgentCli::command();
        let _ = command.print_help();
        eprintln!();
        std::process::exit(2);
    }

    match run(cli) {
        Ok(exit_code) => std::process::exit(exit_code),
        Err(err) => {
            crate::display_error(&err);
            std::process::exit(2);
        }
    }
}

pub fn run(cli: Packet28AgentCli) -> Result<i32> {
    let root = resolve_root_arg(&cli.root)?;
    let preflight_path = crate::agent_surface::latest_preflight_path(&root);
    let cached_coverage_state = root.join(".covy").join("state").join("latest.bin");
    let preflight_parent = preflight_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("invalid preflight output path"))?;
    fs::create_dir_all(preflight_parent).with_context(|| {
        format!(
            "failed to create Packet28 agent directory '{}'",
            preflight_parent.display()
        )
    })?;

    let preflight_args = crate::cmd_preflight::PreflightArgs {
        task: cli.task,
        root: root.to_string_lossy().into_owned(),
        task_id: cli.task_id,
        base: None,
        head: None,
        budget_tokens: 5_000,
        limit_recall: 4,
        focus_paths: Vec::new(),
        focus_symbols: Vec::new(),
        coverage: Vec::new(),
        stack_input: None,
        build_input: None,
        testmap: ".covy/state/testmap.bin".to_string(),
        include: Vec::new(),
        exclude: wrapper_excludes(&cached_coverage_state),
        json: Some(cli.json),
        pretty: false,
    };
    let preflight_value = crate::cmd_preflight::execute_local_json(preflight_args, "covy.toml")?;
    fs::write(&preflight_path, serde_json::to_vec(&preflight_value)?).with_context(|| {
        format!(
            "failed to persist preflight payload to '{}'",
            preflight_path.display()
        )
    })?;

    let mut child = Command::new(&cli.command[0]);
    child
        .args(&cli.command[1..])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .env("PACKET28_PREFLIGHT_PATH", &preflight_path)
        .env("PACKET28_ROOT", &root);
    let status = child
        .status()
        .with_context(|| format!("failed to execute delegated command '{}'", cli.command[0]))?;
    Ok(status.code().unwrap_or(1))
}

fn wrapper_excludes(
    cached_coverage_state: &std::path::Path,
) -> Vec<crate::cmd_preflight::PreflightReducer> {
    if cached_coverage_state.exists() {
        Vec::new()
    } else {
        vec![crate::cmd_preflight::PreflightReducer::Diff]
    }
}

fn resolve_root_arg(root: &str) -> Result<PathBuf> {
    let cwd = crate::cmd_common::caller_cwd()?;
    Ok(PathBuf::from(crate::cmd_common::resolve_path_from_cwd(
        root, &cwd,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapper_parser_keeps_child_args_after_separator() {
        let cli = Packet28AgentCli::try_parse_from([
            "packet28-agent",
            "--task",
            "investigate parser regression",
            "--",
            "codex",
            "--model",
            "gpt-5",
        ])
        .unwrap();
        assert_eq!(cli.command, vec!["codex", "--model", "gpt-5"]);
        assert_eq!(cli.json, crate::cmd_common::JsonProfileArg::Compact);
    }

    #[test]
    fn wrapper_excludes_diff_without_cached_coverage_state() {
        let excludes = wrapper_excludes(std::path::Path::new("/tmp/does-not-exist"));
        assert_eq!(excludes, vec![crate::cmd_preflight::PreflightReducer::Diff]);
    }
}
