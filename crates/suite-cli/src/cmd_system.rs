use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{bail, Context, Result};
use clap::{Args, ValueEnum};
use packet28_reducer_core::{classify_command_argv, reduce_command_output};
use regex::Regex;

mod deps;
mod json;
mod output;
mod search;
mod source;
mod summary;

use deps::summarize_deps_dir;
use json::{filter_json_compact, filter_json_schema, validate_json_extension};
use output::{emit_system_output, summarize_rendered_lines, SystemOutput};
pub use search::{run_find, run_grep};
pub use source::{run_read, run_smart};
use summary::summarize_command_output;

#[cfg(test)]
use search::{parse_find_args, render_find_results, render_grep_results, ParsedFindArgs};
#[cfg(test)]
use source::{
    apply_line_window, filter_read_content, format_with_line_numbers, summarize_source_file,
    ReadLanguage,
};

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ReadFilterLevel {
    None,
    Minimal,
    Aggressive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum GrepEngine {
    Auto,
    Indexed,
    Legacy,
    Fff,
}

#[derive(Args)]
pub struct ReadArgs {
    /// Files to read. Use - to read stdin.
    #[arg(required = true, num_args = 1..)]
    pub files: Vec<PathBuf>,

    /// Filter level: none, minimal, or aggressive
    #[arg(short, long, value_enum, default_value_t = ReadFilterLevel::None)]
    pub level: ReadFilterLevel,

    /// Maximum lines to show from the start after filtering
    #[arg(short, long, conflicts_with = "tail_lines")]
    pub max_lines: Option<usize>,

    /// Show only the last N lines after filtering
    #[arg(long, conflicts_with = "max_lines")]
    pub tail_lines: Option<usize>,

    /// Show line numbers
    #[arg(short = 'n', long)]
    pub line_numbers: bool,

    /// Emit a machine-readable envelope
    #[arg(long)]
    pub json: bool,

    /// Pretty-print JSON machine output
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Args)]
pub struct SummaryArgs {
    /// Command to run and summarize
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    pub command: Vec<String>,

    /// Emit a machine-readable envelope
    #[arg(long)]
    pub json: bool,

    /// Pretty-print JSON machine output
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Args)]
pub struct FormatterArgs {
    /// Formatter arguments
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,

    /// Emit a machine-readable envelope
    #[arg(long)]
    pub json: bool,

    /// Pretty-print JSON machine output
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Args)]
pub struct ToolArgs {
    /// Tool arguments
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,

    /// Emit a machine-readable envelope
    #[arg(long)]
    pub json: bool,

    /// Pretty-print JSON machine output
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Args)]
pub struct SmartArgs {
    /// File to summarize with local heuristics
    pub file: PathBuf,

    /// Compatibility knob for RTK's model flag. Packet28 uses local heuristics.
    #[arg(short, long, default_value = "heuristic")]
    pub model: String,

    /// Compatibility knob for RTK's force-download flag. Packet28 performs no download.
    #[arg(long)]
    pub force_download: bool,

    /// Emit a machine-readable envelope
    #[arg(long)]
    pub json: bool,

    /// Pretty-print JSON machine output
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Args)]
pub struct FindArgs {
    /// Find arguments. Supports `find <pattern> [path]` and native `find <path> -name <pattern>`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,

    /// Emit a machine-readable envelope
    #[arg(long)]
    pub json: bool,

    /// Pretty-print JSON machine output
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Args)]
pub struct GrepArgs {
    /// Pattern to search for
    pub pattern: String,

    /// File or directory to search
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// Maximum line length to render
    #[arg(short = 'l', long, default_value_t = 80)]
    pub max_len: usize,

    /// Maximum result lines to render
    #[arg(short, long, default_value_t = 200)]
    pub max: usize,

    /// Show a short context slice around the match
    #[arg(long)]
    pub context_only: bool,

    /// Filter by extension, such as rs, py, or ts
    #[arg(short = 't', long)]
    pub file_type: Option<String>,

    /// Search backend to use. `fff` delegates to the existing p28 fff-mcp adapter.
    #[arg(long, value_enum, default_value_t = GrepEngine::Auto)]
    pub engine: GrepEngine,

    /// Emit a machine-readable envelope
    #[arg(long)]
    pub json: bool,

    /// Pretty-print JSON machine output
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Args)]
pub struct JsonArgs {
    /// JSON file to inspect. Reads stdin when omitted.
    pub file: Option<PathBuf>,

    /// Maximum nesting depth to render before eliding children
    #[arg(long, default_value_t = 5)]
    pub max_depth: usize,

    /// Render type/schema information without scalar values
    #[arg(long, alias = "keys-only")]
    pub schema_only: bool,

    /// Emit a machine-readable envelope
    #[arg(long)]
    pub json: bool,

    /// Pretty-print JSON machine output
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Args)]
pub struct DepsArgs {
    /// Project directory or file inside a project
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// Emit a machine-readable envelope
    #[arg(long)]
    pub json: bool,

    /// Pretty-print JSON machine output
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Args)]
pub struct EnvArgs {
    /// Only show variables whose names contain this text
    pub filter: Option<String>,

    /// Show sensitive values instead of masking them
    #[arg(long)]
    pub show_all: bool,

    /// Emit a machine-readable envelope
    #[arg(long)]
    pub json: bool,

    /// Pretty-print JSON machine output
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Args)]
pub struct LogArgs {
    /// Log file to analyze. Reads stdin when omitted.
    pub file: Option<PathBuf>,

    /// Emit a machine-readable envelope
    #[arg(long)]
    pub json: bool,

    /// Pretty-print JSON machine output
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Args)]
pub struct PipeArgs {
    /// Named filter to apply. Uses local auto-detection when omitted.
    #[arg(short, long)]
    pub filter: Option<String>,

    /// Pass stdin through without filtering
    #[arg(long)]
    pub passthrough: bool,
}

pub fn run_summary(args: SummaryArgs) -> Result<i32> {
    run_summary_labeled(args, "summary")
}

pub fn run_err(args: SummaryArgs) -> Result<i32> {
    run_summary_labeled(args, "err")
}

pub fn run_prettier(args: FormatterArgs) -> Result<i32> {
    run_formatter_command(
        FormatterInvocation::new("prettier", Vec::new()),
        args,
        "prettier",
    )
}

pub fn run_format(args: FormatterArgs) -> Result<i32> {
    let formatter = detect_formatter_invocation(&args.args)?;
    run_formatter_command(formatter, args, "format")
}

pub fn run_npm(args: ToolArgs) -> Result<i32> {
    run_tool_command("npm", args, "npm")
}

pub fn run_npx(args: ToolArgs) -> Result<i32> {
    run_tool_command("npx", args, "npx")
}

pub fn run_tsc(args: ToolArgs) -> Result<i32> {
    run_tool_command("tsc", args, "tsc")
}

pub fn run_vitest(args: ToolArgs) -> Result<i32> {
    run_test_reducer_tool_command("vitest", args, "vitest")
}

pub fn run_pytest(args: ToolArgs) -> Result<i32> {
    run_tool_command("pytest", args, "pytest")
}

pub fn run_ruff(args: ToolArgs) -> Result<i32> {
    run_tool_command("ruff", args, "ruff")
}

pub fn run_ls(args: ToolArgs) -> Result<i32> {
    run_reducer_tool_command("ls", args, "ls")
}

pub fn run_tree(args: ToolArgs) -> Result<i32> {
    run_reducer_tool_command("tree", args, "tree")
}

pub fn run_wc(args: ToolArgs) -> Result<i32> {
    run_reducer_tool_command("wc", args, "wc")
}

pub fn run_wget(args: ToolArgs) -> Result<i32> {
    run_reducer_tool_command("wget", args, "wget")
}

pub fn run_curl(args: ToolArgs) -> Result<i32> {
    run_reducer_tool_command("curl", args, "curl")
}

pub fn run_docker(args: ToolArgs) -> Result<i32> {
    run_reducer_tool_command("docker", args, "docker")
}

pub fn run_kubectl(args: ToolArgs) -> Result<i32> {
    run_reducer_tool_command("kubectl", args, "kubectl")
}

pub fn run_gh(args: ToolArgs) -> Result<i32> {
    run_reducer_tool_command("gh", args, "gh")
}

pub fn run_glab(args: ToolArgs) -> Result<i32> {
    run_reducer_tool_command("glab", args, "glab")
}

pub fn run_aws(args: ToolArgs) -> Result<i32> {
    run_reducer_tool_command("aws", args, "aws")
}

pub fn run_psql(args: ToolArgs) -> Result<i32> {
    run_reducer_tool_command("psql", args, "psql")
}

pub fn run_git(args: ToolArgs) -> Result<i32> {
    run_reducer_tool_command("git", args, "git")
}

pub fn run_gt(args: ToolArgs) -> Result<i32> {
    run_reducer_tool_command("gt", args, "gt")
}

pub fn run_cargo(args: ToolArgs) -> Result<i32> {
    run_reducer_tool_command("cargo", args, "cargo")
}

pub fn run_go(args: ToolArgs) -> Result<i32> {
    run_reducer_tool_command("go", args, "go")
}

pub fn run_dotnet(args: ToolArgs) -> Result<i32> {
    run_reducer_tool_command("dotnet", args, "dotnet")
}

pub fn run_golangci_lint(args: ToolArgs) -> Result<i32> {
    run_reducer_tool_command("golangci-lint", args, "golangci-lint")
}

pub fn run_gradlew(args: ToolArgs) -> Result<i32> {
    run_reducer_tool_command("gradlew", args, "gradlew")
}

pub fn run_rake(args: ToolArgs) -> Result<i32> {
    run_reducer_tool_command("rake", args, "rake")
}

pub fn run_rspec(args: ToolArgs) -> Result<i32> {
    run_reducer_tool_command("rspec", args, "rspec")
}

pub fn run_rubocop(args: ToolArgs) -> Result<i32> {
    run_reducer_tool_command("rubocop", args, "rubocop")
}

pub fn run_mypy(args: ToolArgs) -> Result<i32> {
    run_reducer_tool_command("mypy", args, "mypy")
}

pub fn run_pip(args: ToolArgs) -> Result<i32> {
    run_reducer_tool_command("pip", args, "pip")
}

pub fn run_pnpm(args: ToolArgs) -> Result<i32> {
    run_reducer_tool_command("pnpm", args, "pnpm")
}

pub fn run_bundle(args: ToolArgs) -> Result<i32> {
    run_reducer_tool_command("bundle", args, "bundle")
}

pub fn run_brew(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("brew", args, "brew")
}

pub fn run_composer(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("composer", args, "composer")
}

pub fn run_df(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("df", args, "df")
}

pub fn run_du(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("du", args, "du")
}

pub fn run_make(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("make", args, "make")
}

pub fn run_mvn(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("mvn", args, "mvn")
}

pub fn run_poetry(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("poetry", args, "poetry")
}

pub fn run_uv(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("uv", args, "uv")
}

pub fn run_terraform(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("terraform", args, "terraform")
}

pub fn run_tofu(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("tofu", args, "tofu")
}

pub fn run_ansible_playbook(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("ansible-playbook", args, "ansible-playbook")
}

pub fn run_fail2ban_client(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("fail2ban-client", args, "fail2ban-client")
}

pub fn run_gcloud(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("gcloud", args, "gcloud")
}

pub fn run_hadolint(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("hadolint", args, "hadolint")
}

pub fn run_helm(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("helm", args, "helm")
}

pub fn run_iptables(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("iptables", args, "iptables")
}

pub fn run_markdownlint(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("markdownlint", args, "markdownlint")
}

pub fn run_mix(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("mix", args, "mix")
}

pub fn run_ping(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("ping", args, "ping")
}

pub fn run_pio(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("pio", args, "pio")
}

pub fn run_pre_commit(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("pre-commit", args, "pre-commit")
}

pub fn run_ps(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("ps", args, "ps")
}

pub fn run_quarto(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("quarto", args, "quarto")
}

pub fn run_rsync(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("rsync", args, "rsync")
}

pub fn run_shellcheck(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("shellcheck", args, "shellcheck")
}

pub fn run_shopify(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("shopify", args, "shopify")
}

pub fn run_sops(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("sops", args, "sops")
}

pub fn run_systemctl(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("systemctl", args, "systemctl")
}

pub fn run_trunk(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("trunk", args, "trunk")
}

pub fn run_yamllint(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("yamllint", args, "yamllint")
}

pub fn run_liquibase(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("liquibase", args, "liquibase")
}

pub fn run_basedpyright(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("basedpyright", args, "basedpyright")
}

pub fn run_biome(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("biome", args, "biome")
}

pub fn run_gcc(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("gcc", args, "gcc")
}

pub fn run_gpp(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("g++", args, "g++")
}

pub fn run_gradle(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("gradle", args, "gradle")
}

pub fn run_jira(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("jira", args, "jira")
}

pub fn run_jj(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("jj", args, "jj")
}

pub fn run_jq(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("jq", args, "jq")
}

pub fn run_just(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("just", args, "just")
}

pub fn run_mise(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("mise", args, "mise")
}

pub fn run_nx(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("nx", args, "nx")
}

pub fn run_ollama(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("ollama", args, "ollama")
}

pub fn run_oxlint(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("oxlint", args, "oxlint")
}

pub fn run_skopeo(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("skopeo", args, "skopeo")
}

pub fn run_ssh(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("ssh", args, "ssh")
}

pub fn run_stat(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("stat", args, "stat")
}

pub fn run_swift(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("swift", args, "swift")
}

pub fn run_task(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("task", args, "task")
}

pub fn run_turbo(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("turbo", args, "turbo")
}

pub fn run_ty(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("ty", args, "ty")
}

pub fn run_xcodebuild(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("xcodebuild", args, "xcodebuild")
}

pub fn run_yadm(args: ToolArgs) -> Result<i32> {
    run_filtered_tool_command("yadm", args, "yadm")
}

fn run_summary_labeled(args: SummaryArgs, label: &str) -> Result<i32> {
    let output = execute_command(&args.command)?;
    let raw = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let exit_code = output.status.code().unwrap_or(1);
    let text = summarize_command_output(&raw, &args.command.join(" "), output.status.success());
    emit_system_output(
        args.json,
        args.pretty,
        SystemOutput {
            command: format!("Packet28 {label}"),
            summary: summarize_rendered_lines(label, &text),
            text,
        },
    )?;
    Ok(exit_code)
}

fn execute_command(command: &[String]) -> Result<Output> {
    let (program, rest) = command
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("command is required"))?;
    Command::new(program)
        .args(rest)
        .output()
        .with_context(|| format!("failed to execute '{}'", command.join(" ")))
}

fn run_tool_command(program: &str, args: ToolArgs, label: &str) -> Result<i32> {
    let mut command = Vec::with_capacity(args.args.len() + 1);
    command.push(program.to_string());
    command.extend(args.args);
    run_summary_labeled(
        SummaryArgs {
            command,
            json: args.json,
            pretty: args.pretty,
        },
        label,
    )
}

fn run_reducer_tool_command(program: &str, args: ToolArgs, label: &str) -> Result<i32> {
    let mut command = Vec::with_capacity(args.args.len() + 1);
    command.push(program.to_string());
    command.extend(args.args);
    let output = execute_command(&command)?;
    let exit_code = output.status.code().unwrap_or(1);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let text = if let Some(spec) = classify_command_argv(&command.join(" "), &command) {
        let reduction = reduce_command_output(&spec, &stdout, &stderr, exit_code)?;
        if reduction.compact_preview.trim().is_empty() {
            reduction.summary
        } else {
            format!(
                "{}\n{}",
                reduction.summary,
                reduction.compact_preview.trim_end()
            )
        }
    } else {
        summarize_command_output(
            &format!("{stdout}{stderr}"),
            &command.join(" "),
            output.status.success(),
        )
    };
    emit_system_output(
        args.json,
        args.pretty,
        SystemOutput {
            command: format!("Packet28 {label}"),
            summary: summarize_rendered_lines(label, &text),
            text,
        },
    )?;
    Ok(exit_code)
}

/// Runs a test tool once and renders its dedicated reducer. A failed run whose
/// output yields no failing test keeps its raw lines instead of a guessed summary.
fn run_test_reducer_tool_command(program: &str, args: ToolArgs, label: &str) -> Result<i32> {
    let mut command = Vec::with_capacity(args.args.len() + 1);
    command.push(program.to_string());
    command.extend(args.args);
    let output = execute_command(&command)?;
    let exit_code = output.status.code().unwrap_or(1);
    let success = output.status.success();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let command_text = command.join(" ");
    let reduction = match classify_command_argv(&command_text, &command) {
        Some(spec) => Some(reduce_command_output(&spec, &stdout, &stderr, exit_code)?),
        None => None,
    };
    let text = match reduction {
        Some(reduction) if success || !reduction.compact_preview.trim().is_empty() => {
            let mut rendered = vec![
                command_status_line(&command_text, success),
                reduction.summary,
            ];
            if !reduction.compact_preview.trim().is_empty() {
                rendered.push(reduction.compact_preview.trim_end().to_string());
            }
            rendered.join("\n")
        }
        Some(_) => render_unrecognized_failure(&command_text, &format!("{stdout}{stderr}")),
        None => summarize_command_output(&format!("{stdout}{stderr}"), &command_text, success),
    };
    emit_system_output(
        args.json,
        args.pretty,
        SystemOutput {
            command: format!("Packet28 {label}"),
            summary: summarize_rendered_lines(label, &text),
            text,
        },
    )?;
    Ok(exit_code)
}

const UNRECOGNIZED_FAILURE_HEAD_LINES: usize = 10;
const UNRECOGNIZED_FAILURE_TAIL_LINES: usize = 30;

fn command_status_line(command: &str, success: bool) -> String {
    format!(
        "{} Command: {}",
        if success { "[ok]" } else { "[FAIL]" },
        compact_for_log(command, 80)
    )
}

fn render_unrecognized_failure(command: &str, raw: &str) -> String {
    let lines = raw
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    let mut rendered = vec![
        command_status_line(command, false),
        "   output not recognized; showing raw lines".to_string(),
    ];
    let limit = UNRECOGNIZED_FAILURE_HEAD_LINES + UNRECOGNIZED_FAILURE_TAIL_LINES;
    if lines.len() <= limit {
        rendered.extend(lines.iter().map(|line| line.to_string()));
    } else {
        let tail_start = lines.len() - UNRECOGNIZED_FAILURE_TAIL_LINES;
        rendered.extend(
            lines[..UNRECOGNIZED_FAILURE_HEAD_LINES]
                .iter()
                .map(|line| line.to_string()),
        );
        rendered.push(format!(
            "   ... {} line(s) omitted",
            tail_start - UNRECOGNIZED_FAILURE_HEAD_LINES
        ));
        rendered.extend(lines[tail_start..].iter().map(|line| line.to_string()));
    }
    rendered.join("\n")
}

fn run_filtered_tool_command(program: &str, args: ToolArgs, label: &str) -> Result<i32> {
    let mut command = Vec::with_capacity(args.args.len() + 1);
    command.push(program.to_string());
    command.extend(args.args);
    let output = execute_command(&command)?;
    let exit_code = output.status.code().unwrap_or(1);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let command_text = command.join(" ");
    let cwd = env::current_dir().context("failed to read current directory")?;
    let text = if let Some(filter) =
        crate::toml_filters::apply_configured_filter(&cwd, &command_text, &stdout, &stderr)?
    {
        filter.output
    } else {
        summarize_command_output(
            &format!("{stdout}{stderr}"),
            &command_text,
            output.status.success(),
        )
    };
    emit_system_output(
        args.json,
        args.pretty,
        SystemOutput {
            command: format!("Packet28 {label}"),
            summary: summarize_rendered_lines(label, &text),
            text,
        },
    )?;
    Ok(exit_code)
}

struct FormatterInvocation {
    program: String,
    prefix_args: Vec<String>,
}

impl FormatterInvocation {
    fn new(program: &str, prefix_args: Vec<String>) -> Self {
        Self {
            program: program.to_string(),
            prefix_args,
        }
    }
}

fn run_formatter_command(
    invocation: FormatterInvocation,
    args: FormatterArgs,
    label: &str,
) -> Result<i32> {
    let mut command = Vec::with_capacity(args.args.len() + 1 + invocation.prefix_args.len());
    command.push(invocation.program.clone());
    command.extend(invocation.prefix_args);
    let rest = if label == "format" && args.args.first().is_some_and(|arg| is_formatter_name(arg)) {
        &args.args[1..]
    } else {
        args.args.as_slice()
    };
    command.extend(rest.iter().cloned());
    run_summary_labeled(
        SummaryArgs {
            command,
            json: args.json,
            pretty: args.pretty,
        },
        label,
    )
}

fn detect_formatter_invocation(args: &[String]) -> Result<FormatterInvocation> {
    if let Some(first) = args.first() {
        if first == "ruff" {
            let prefix = if args.get(1).is_some_and(|arg| arg == "format") {
                Vec::new()
            } else {
                vec!["format".to_string()]
            };
            return Ok(FormatterInvocation::new("ruff", prefix));
        }
        if is_formatter_name(first) {
            return Ok(FormatterInvocation::new(first, Vec::new()));
        }
    }
    if Path::new("package.json").exists()
        || Path::new(".prettierrc").exists()
        || Path::new(".prettierrc.json").exists()
        || Path::new(".prettierrc.js").exists()
    {
        return Ok(FormatterInvocation::new("prettier", Vec::new()));
    }
    if let Ok(pyproject) = fs::read_to_string("pyproject.toml") {
        if pyproject.contains("[tool.ruff") {
            return Ok(FormatterInvocation::new("ruff", vec!["format".to_string()]));
        }
        if pyproject.contains("[tool.black]") {
            return Ok(FormatterInvocation::new("black", Vec::new()));
        }
    }
    Ok(FormatterInvocation::new("prettier", Vec::new()))
}

fn is_formatter_name(value: &str) -> bool {
    matches!(value, "prettier" | "black" | "ruff" | "biome")
}

pub fn run_json(args: JsonArgs) -> Result<i32> {
    let content = match &args.file {
        Some(path) => {
            validate_json_extension(path)?;
            fs::read_to_string(path)
                .with_context(|| format!("failed to read JSON file {}", path.display()))?
        }
        None => {
            let mut content = String::new();
            io::stdin()
                .lock()
                .read_to_string(&mut content)
                .context("failed to read JSON from stdin")?;
            content
        }
    };

    let text = if args.schema_only {
        filter_json_schema(&content, args.max_depth)?
    } else {
        filter_json_compact(&content, args.max_depth)?
    };
    let summary = summarize_rendered_lines("JSON", &text);
    emit_system_output(
        args.json,
        args.pretty,
        SystemOutput {
            command: "Packet28 json".to_string(),
            summary,
            text,
        },
    )
}

pub fn run_deps(args: DepsArgs) -> Result<i32> {
    let dir = if args.path.is_file() {
        args.path.parent().unwrap_or(Path::new(".")).to_path_buf()
    } else {
        args.path.clone()
    };
    let text = summarize_deps_dir(&dir)?;
    let summary = summarize_rendered_lines("dependencies", &text);
    emit_system_output(
        args.json,
        args.pretty,
        SystemOutput {
            command: "Packet28 deps".to_string(),
            summary,
            text,
        },
    )
}

pub fn run_env(args: EnvArgs) -> Result<i32> {
    let mut vars = env::vars().collect::<Vec<_>>();
    vars.sort_by(|a, b| a.0.cmp(&b.0));
    let text = render_env_vars(&vars, args.filter.as_deref(), args.show_all);
    let summary = summarize_rendered_lines("environment", &text);
    emit_system_output(
        args.json,
        args.pretty,
        SystemOutput {
            command: "Packet28 env".to_string(),
            summary,
            text,
        },
    )
}

pub fn run_log(args: LogArgs) -> Result<i32> {
    let content = match &args.file {
        Some(path) => fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?,
        None => {
            let mut content = String::new();
            io::stdin()
                .lock()
                .read_to_string(&mut content)
                .context("failed to read log input from stdin")?;
            content
        }
    };
    let text = analyze_logs(&content)?;
    let summary = summarize_rendered_lines("logs", &text);
    emit_system_output(
        args.json,
        args.pretty,
        SystemOutput {
            command: "Packet28 log".to_string(),
            summary,
            text,
        },
    )
}

pub fn run_pipe(args: PipeArgs) -> Result<i32> {
    let mut input = String::new();
    io::stdin()
        .read_to_string(&mut input)
        .context("failed to read pipe input from stdin")?;
    if args.passthrough {
        print!("{input}");
        return Ok(0);
    }
    let output = filter_pipe_input(args.filter.as_deref(), &input)?;
    print!("{output}");
    Ok(0)
}

fn render_env_vars(vars: &[(String, String)], filter: Option<&str>, show_all: bool) -> String {
    let sensitive_patterns = sensitive_patterns();
    let filter = filter.map(str::to_lowercase);
    let mut path_vars = Vec::new();
    let mut lang_vars = Vec::new();
    let mut cloud_vars = Vec::new();
    let mut tool_vars = Vec::new();
    let mut other_vars = Vec::new();

    for (key, value) in vars {
        if let Some(filter) = &filter {
            if !key.to_lowercase().contains(filter) {
                continue;
            }
        }
        let sensitive = sensitive_patterns
            .iter()
            .any(|pattern| key.to_lowercase().contains(pattern));
        let display_value = if sensitive && !show_all {
            mask_value(value)
        } else if value.chars().count() > 100 {
            let preview = value.chars().take(50).collect::<String>();
            format!("{preview}... ({} chars)", value.chars().count())
        } else {
            value.clone()
        };
        let entry = (key.clone(), display_value);
        if key.contains("PATH") {
            path_vars.push(entry);
        } else if is_lang_var(key) {
            lang_vars.push(entry);
        } else if is_cloud_var(key) {
            cloud_vars.push(entry);
        } else if is_tool_var(key) {
            tool_vars.push(entry);
        } else if filter.is_some() || is_interesting_var(key) {
            other_vars.push(entry);
        }
    }

    let mut out = String::new();
    append_path_vars(&mut out, &path_vars);
    append_env_section(&mut out, "Language/Runtime", &lang_vars, None);
    append_env_section(&mut out, "Cloud/Services", &cloud_vars, None);
    append_env_section(&mut out, "Tools", &tool_vars, None);
    append_env_section(&mut out, "Other", &other_vars, Some(20));
    if filter.is_none() {
        let shown = path_vars.len()
            + lang_vars.len()
            + cloud_vars.len()
            + tool_vars.len()
            + other_vars.len().min(20);
        out.push_str(&format!(
            "\nTotal: {} vars (showing {} relevant)\n",
            vars.len(),
            shown
        ));
    }
    out
}

fn append_path_vars(out: &mut String, vars: &[(String, String)]) {
    if vars.is_empty() {
        return;
    }
    out.push_str("PATH Variables:\n");
    for (key, value) in vars {
        if key == "PATH" {
            let paths = value.split(':').collect::<Vec<_>>();
            out.push_str(&format!("  PATH ({} entries):\n", paths.len()));
            for path in paths.iter().take(5) {
                out.push_str(&format!("    {path}\n"));
            }
            if paths.len() > 5 {
                out.push_str(&format!("    ... +{} more\n", paths.len() - 5));
            }
        } else {
            out.push_str(&format!("  {key}={value}\n"));
        }
    }
}

fn append_env_section(
    out: &mut String,
    label: &str,
    vars: &[(String, String)],
    limit: Option<usize>,
) {
    if vars.is_empty() {
        return;
    }
    out.push_str(&format!("\n{label}:\n"));
    let limit = limit.unwrap_or(vars.len());
    for (key, value) in vars.iter().take(limit) {
        out.push_str(&format!("  {key}={value}\n"));
    }
    if vars.len() > limit {
        out.push_str(&format!("  ... +{} more\n", vars.len() - limit));
    }
}

fn sensitive_patterns() -> HashSet<&'static str> {
    [
        "key",
        "secret",
        "password",
        "token",
        "credential",
        "auth",
        "private",
        "api_key",
        "apikey",
        "access_key",
        "jwt",
    ]
    .into_iter()
    .collect()
}

fn mask_value(value: &str) -> String {
    let chars = value.chars().collect::<Vec<_>>();
    if chars.len() <= 4 {
        "****".to_string()
    } else {
        let prefix = chars.iter().take(2).collect::<String>();
        let suffix = chars
            .iter()
            .skip(chars.len().saturating_sub(2))
            .collect::<String>();
        format!("{prefix}****{suffix}")
    }
}

fn is_lang_var(key: &str) -> bool {
    const PATTERNS: &[&str] = &[
        "RUST", "CARGO", "PYTHON", "PIP", "NODE", "NPM", "YARN", "DENO", "BUN", "JAVA", "MAVEN",
        "GRADLE", "GO", "GOPATH", "GOROOT", "RUBY", "GEM", "PERL", "PHP", "DOTNET", "NUGET",
    ];
    let key = key.to_uppercase();
    PATTERNS.iter().any(|pattern| key.contains(pattern))
}

fn is_cloud_var(key: &str) -> bool {
    const PATTERNS: &[&str] = &[
        "AWS",
        "AZURE",
        "GCP",
        "GOOGLE_CLOUD",
        "DOCKER",
        "KUBERNETES",
        "K8S",
        "HELM",
        "TERRAFORM",
        "VAULT",
        "CONSUL",
        "NOMAD",
    ];
    let key = key.to_uppercase();
    PATTERNS.iter().any(|pattern| key.contains(pattern))
}

fn is_tool_var(key: &str) -> bool {
    const PATTERNS: &[&str] = &[
        "EDITOR",
        "VISUAL",
        "SHELL",
        "TERM",
        "GIT",
        "SSH",
        "GPG",
        "BREW",
        "HOMEBREW",
        "XDG",
        "CLAUDE",
        "ANTHROPIC",
    ];
    let key = key.to_uppercase();
    PATTERNS.iter().any(|pattern| key.contains(pattern))
}

fn is_interesting_var(key: &str) -> bool {
    const PATTERNS: &[&str] = &["HOME", "USER", "LANG", "LC_", "TZ", "PWD", "OLDPWD"];
    let key = key.to_uppercase();
    PATTERNS.iter().any(|pattern| key.starts_with(pattern))
}

fn analyze_logs(content: &str) -> Result<String> {
    let timestamp_re = Regex::new(r"^\d{4}[-/]\d{2}[-/]\d{2}[T ]\d{2}:\d{2}:\d{2}[.,]?\d*\s*")?;
    let uuid_re =
        Regex::new(r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}")?;
    let hex_re = Regex::new(r"0x[0-9a-fA-F]+")?;
    let num_re = Regex::new(r"\b\d{4,}\b")?;
    let path_re = Regex::new(r"/[\w./\-]+")?;

    let mut error_counts = std::collections::HashMap::<String, usize>::new();
    let mut warn_counts = std::collections::HashMap::<String, usize>::new();
    let mut info_counts = std::collections::HashMap::<String, usize>::new();
    let mut unique_errors = Vec::<String>::new();
    let mut unique_warnings = Vec::<String>::new();

    for line in content.lines() {
        let lower = line.to_lowercase();
        let normalized =
            normalize_log_line(line, &timestamp_re, &uuid_re, &hex_re, &num_re, &path_re);
        if lower.contains("error") || lower.contains("fatal") || lower.contains("panic") {
            let count = error_counts.entry(normalized).or_insert(0);
            if *count == 0 {
                unique_errors.push(line.to_string());
            }
            *count += 1;
        } else if lower.contains("warn") {
            let count = warn_counts.entry(normalized).or_insert(0);
            if *count == 0 {
                unique_warnings.push(line.to_string());
            }
            *count += 1;
        } else if lower.contains("info") {
            *info_counts.entry(normalized).or_insert(0) += 1;
        }
    }

    let total_errors = error_counts.values().sum::<usize>();
    let total_warnings = warn_counts.values().sum::<usize>();
    let total_info = info_counts.values().sum::<usize>();
    let mut lines = vec![
        "Log Summary".to_string(),
        format!(
            "   [error] {} errors ({} unique)",
            total_errors,
            error_counts.len()
        ),
        format!(
            "   [warn] {} warnings ({} unique)",
            total_warnings,
            warn_counts.len()
        ),
        format!("   [info] {} info messages", total_info),
        String::new(),
    ];
    append_log_group(
        &mut lines,
        "[ERRORS]",
        &error_counts,
        &unique_errors,
        (&timestamp_re, &uuid_re, &hex_re, &num_re, &path_re),
        10,
    );
    append_log_group(
        &mut lines,
        "[WARNINGS]",
        &warn_counts,
        &unique_warnings,
        (&timestamp_re, &uuid_re, &hex_re, &num_re, &path_re),
        5,
    );
    Ok(lines.join("\n"))
}

fn filter_pipe_input(filter: Option<&str>, input: &str) -> Result<String> {
    let selected = filter
        .map(|name| name.to_string())
        .unwrap_or_else(|| auto_detect_pipe_filter(input).to_string());
    match selected.as_str() {
        "grep" | "rg" => Ok(render_pipe_grep(input)),
        "find" | "fd" => Ok(render_pipe_find(input)),
        "json" => filter_json_compact(input, 4),
        "log" => analyze_logs(input),
        "cargo" | "cargo-test" => Ok(summarize_command_output(input, "cargo test", true)),
        "pytest" => Ok(summarize_command_output(input, "pytest", true)),
        "go-test" => Ok(summarize_command_output(input, "go test", true)),
        "go-build" => Ok(summarize_command_output(input, "go build", true)),
        "tsc" => Ok(summarize_command_output(input, "tsc", true)),
        "vitest" => Ok(summarize_command_output(input, "vitest", true)),
        "mypy" => Ok(summarize_command_output(input, "mypy", true)),
        "ruff-check" => Ok(summarize_command_output(input, "ruff check", true)),
        "ruff-format" => Ok(summarize_command_output(input, "ruff format", true)),
        "prettier" => Ok(summarize_command_output(input, "prettier", true)),
        "auto" => Ok(input.to_string()),
        other => bail!(
            "unknown pipe filter '{other}' (available: cargo-test, pytest, go-test, go-build, tsc, vitest, grep, rg, find, fd, json, log, mypy, ruff-check, ruff-format, prettier)"
        ),
    }
}

fn auto_detect_pipe_filter(input: &str) -> &'static str {
    let first = input.lines().take(8).collect::<Vec<_>>().join("\n");
    let lower = first.to_lowercase();
    let trimmed = first.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        "json"
    } else if lower.contains("test result:") || lower.contains("=== test session starts") {
        "cargo-test"
    } else if first.lines().any(is_grep_like_line) {
        "grep"
    } else if looks_like_find_output(&first) {
        "find"
    } else if lower.contains("error:") || lower.contains("warn:") || lower.contains("panic") {
        "log"
    } else {
        "auto"
    }
}

fn render_pipe_grep(input: &str) -> String {
    let mut entries = Vec::<(&str, usize, &str)>::new();
    for line in input.lines() {
        let Some((path, rest)) = line.split_once(':') else {
            continue;
        };
        let Some((line_no, text)) = rest.split_once(':') else {
            continue;
        };
        if let Ok(line_no) = line_no.parse::<usize>() {
            entries.push((path, line_no, text));
        }
    }
    if entries.is_empty() {
        return input.to_string();
    }
    let mut files = HashSet::new();
    for (path, _, _) in &entries {
        files.insert(*path);
    }
    let mut out = format!("{} matches in {} files:\n\n", entries.len(), files.len());
    for (path, line_no, text) in entries.iter().take(50) {
        out.push_str(&format!(
            "{path}:{line_no}:{}\n",
            compact_for_log(text.trim(), 160)
        ));
    }
    if entries.len() > 50 {
        out.push_str(&format!("[+{} more]\n", entries.len() - 50));
    }
    out
}

fn render_pipe_find(input: &str) -> String {
    let paths = input
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    if paths.is_empty() {
        return input.to_string();
    }
    let mut out = format!("{} paths:\n", paths.len());
    for path in paths.iter().take(80) {
        out.push_str(path);
        out.push('\n');
    }
    if paths.len() > 80 {
        out.push_str(&format!("[+{} more]\n", paths.len() - 80));
    }
    out
}

fn is_grep_like_line(line: &str) -> bool {
    let Some((_path, rest)) = line.split_once(':') else {
        return false;
    };
    let Some((line_no, _text)) = rest.split_once(':') else {
        return false;
    };
    line_no.parse::<usize>().is_ok()
}

fn looks_like_find_output(input: &str) -> bool {
    let mut count = 0usize;
    for line in input.lines().filter(|line| !line.trim().is_empty()) {
        count += 1;
        let trimmed = line.trim();
        if trimmed.contains(':')
            || !(trimmed.starts_with('.') || trimmed.starts_with('/') || trimmed.contains('/'))
        {
            return false;
        }
    }
    count >= 3
}

fn append_log_group(
    lines: &mut Vec<String>,
    label: &str,
    counts: &std::collections::HashMap<String, usize>,
    originals: &[String],
    regexes: (&Regex, &Regex, &Regex, &Regex, &Regex),
    limit: usize,
) {
    if originals.is_empty() {
        return;
    }
    lines.push(label.to_string());
    let mut sorted = counts.iter().collect::<Vec<_>>();
    sorted.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    for (normalized, count) in sorted.iter().take(limit) {
        let original = originals
            .iter()
            .find(|line| {
                normalize_log_line(line, regexes.0, regexes.1, regexes.2, regexes.3, regexes.4)
                    == **normalized
            })
            .map(String::as_str)
            .unwrap_or(normalized);
        let truncated = compact_for_log(original, 100);
        if **count > 1 {
            lines.push(format!("   [x{count}] {truncated}"));
        } else {
            lines.push(format!("   {truncated}"));
        }
    }
    if sorted.len() > limit {
        lines.push(format!(
            "   ... +{} more unique entries",
            sorted.len() - limit
        ));
    }
    lines.push(String::new());
}

fn normalize_log_line(
    line: &str,
    timestamp_re: &Regex,
    uuid_re: &Regex,
    hex_re: &Regex,
    num_re: &Regex,
    path_re: &Regex,
) -> String {
    let mut normalized = timestamp_re.replace_all(line, "").to_string();
    normalized = uuid_re.replace_all(&normalized, "<UUID>").to_string();
    normalized = hex_re.replace_all(&normalized, "<HEX>").to_string();
    normalized = num_re.replace_all(&normalized, "<NUM>").to_string();
    normalized = path_re.replace_all(&normalized, "<PATH>").to_string();
    normalized.trim().to_string()
}

fn compact_for_log(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        value.to_string()
    } else {
        let budget = max_chars.saturating_sub(3);
        format!("{}...", value.chars().take(budget).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_compact_truncates_values_and_summarizes_arrays() {
        let rendered = filter_json_compact(
            &serde_json::json!({
                "long": "x".repeat(100),
                "items": [1, 2, 3, 4, 5, 6],
            })
            .to_string(),
            5,
        )
        .unwrap();
        assert!(rendered.contains("long: \""));
        assert!(rendered.contains("...\""));
        assert!(rendered.contains("[1, ... +5 more]"));
    }

    #[test]
    fn read_filters_comments_and_applies_line_windows() {
        let content = "// comment\nfn main() {}\n\n// another\nfn next() {}\n";
        let filtered = filter_read_content(content, ReadLanguage::Rust, ReadFilterLevel::Minimal);
        assert!(!filtered.contains("// comment"));
        assert!(filtered.contains("fn main()"));
        let windowed = apply_line_window(&filtered, Some(1), None, ReadLanguage::Rust);
        assert!(windowed.starts_with("fn main()"));
        assert!(windowed.contains("more lines"));
    }

    #[test]
    fn read_line_numbers_are_stable_width() {
        let rendered = format_with_line_numbers("alpha\nbeta\n");
        assert!(rendered.contains("1 | alpha"));
        assert!(rendered.contains("2 | beta"));
    }

    #[test]
    fn smart_summarizes_source_file_with_local_heuristics() {
        let summary = summarize_source_file(
            r#"
use anyhow::Result;

#[derive(Debug)]
pub struct Config {
    name: String,
}

pub fn load_config() -> Result<Config> {
    Ok(Config { name: "demo".to_string() })
}
"#,
            ReadLanguage::Rust,
        );
        assert!(summary.line1.contains("Rust module"));
        assert!(summary.line1.contains("1 fn"));
        assert!(summary.line1.contains("1 type"));
        assert!(summary.line2.contains("derive"));
        assert!(summary.line2.contains("error handling"));
    }

    #[test]
    fn summary_detects_json_and_test_output() {
        let json_summary = summarize_command_output(r#"{"ok":true,"items":[1,2]}"#, "printf", true);
        assert!(json_summary.contains("JSON Summary:"));
        assert!(json_summary.contains("items:"));
        let test_summary = summarize_command_output(
            "test result: FAILED. 1 passed; 2 failed; 3 ignored",
            "cargo test",
            false,
        );
        assert!(test_summary.contains("[FAIL] Command: cargo test"));
        assert!(test_summary.contains("[ok] 1 passed"));
        assert!(test_summary.contains("[FAIL] 2 failed"));
        assert!(test_summary.contains("skip 3 skipped"));
    }

    #[test]
    fn find_parses_native_and_compact_args() {
        let native = parse_find_args(&[
            ".".to_string(),
            "-name".to_string(),
            "*.rs".to_string(),
            "-type".to_string(),
            "f".to_string(),
            "-maxdepth".to_string(),
            "2".to_string(),
        ])
        .unwrap();
        assert_eq!(native.pattern, "*.rs");
        assert_eq!(native.file_type, "f");
        assert_eq!(native.max_depth, Some(2));

        let compact = parse_find_args(&[
            "*.md".to_string(),
            "docs".to_string(),
            "--max".to_string(),
            "5".to_string(),
        ])
        .unwrap();
        assert_eq!(compact.pattern, "*.md");
        assert_eq!(compact.path, PathBuf::from("docs"));
        assert_eq!(compact.max_results, 5);
    }

    #[test]
    fn find_renders_limited_matches() {
        let dir = tempfile::TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src").join("a.rs"), "").unwrap();
        fs::write(dir.path().join("src").join("b.rs"), "").unwrap();
        fs::write(dir.path().join("src").join("c.txt"), "").unwrap();
        let rendered = render_find_results(&ParsedFindArgs {
            pattern: "*.rs".to_string(),
            path: dir.path().to_path_buf(),
            max_results: 1,
            max_depth: Some(3),
            file_type: "f".to_string(),
            case_insensitive: false,
        })
        .unwrap();
        assert!(rendered.contains("2 match(es)"));
        assert!(rendered.contains("[+1 more]"));
    }

    #[test]
    fn grep_groups_matches_and_honors_file_type() {
        let dir = tempfile::TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(
            dir.path().join("src").join("a.rs"),
            "fn alpha() {}\nfn beta() {}\n",
        )
        .unwrap();
        fs::write(dir.path().join("src").join("b.txt"), "fn ignored\n").unwrap();
        let rendered = render_grep_results(&GrepArgs {
            pattern: "fn".to_string(),
            path: dir.path().to_path_buf(),
            max_len: 80,
            max: 1,
            context_only: false,
            file_type: Some("rs".to_string()),
            engine: GrepEngine::Legacy,
            json: false,
            pretty: false,
        })
        .unwrap();
        assert!(rendered.contains("2 matches in 1 files"));
        assert!(rendered.contains("a.rs:1:fn alpha() {}"));
        assert!(rendered.contains("[+1 more]"));
        assert!(!rendered.contains("ignored"));
    }

    #[test]
    fn grep_basic_alternation_matches_like_observed_hook_path() {
        let dir = tempfile::TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(
            dir.path().join("src").join("command.rs"),
            "pub fn classify_command() {}\npub enum Mutation {}\n",
        )
        .unwrap();
        let rendered = render_grep_results(&GrepArgs {
            pattern: r"fn classify\|Mutation".to_string(),
            path: dir.path().join("src").join("command.rs"),
            max_len: 120,
            max: 10,
            context_only: false,
            file_type: None,
            engine: GrepEngine::Legacy,
            json: false,
            pretty: false,
        })
        .unwrap();
        assert!(rendered.contains("2 matches in 1 files"));
        assert!(rendered.contains("command.rs:1:pub fn classify_command() {}"));
        assert!(rendered.contains("command.rs:2:pub enum Mutation {}"));
    }

    #[test]
    fn json_schema_renders_types_without_values() {
        let rendered = filter_json_schema(
            r#"{"count":42,"items":[{"url":"https://example.com","enabled":true}]}"#,
            5,
        )
        .unwrap();
        assert!(rendered.contains("count: int"));
        assert!(rendered.contains("enabled: bool"));
        assert!(rendered.contains("url: url"));
        assert!(!rendered.contains("example.com"));
    }

    #[test]
    fn json_extension_rejects_cargo_toml_with_deps_hint() {
        let err = validate_json_extension(Path::new("Cargo.toml")).unwrap_err();
        assert!(err.to_string().contains("not a JSON file"));
        assert!(err.to_string().contains("Packet28 deps"));
    }

    #[test]
    fn env_masks_sensitive_values_and_limits_path() {
        let vars = vec![
            (
                "AWS_SECRET_ACCESS_KEY".to_string(),
                "supersecrettoken".to_string(),
            ),
            ("PATH".to_string(), "/a:/b:/c:/d:/e:/f".to_string()),
            ("RUST_LOG".to_string(), "debug".to_string()),
        ];
        let rendered = render_env_vars(&vars, None, false);
        assert!(rendered.contains("AWS_SECRET_ACCESS_KEY=su****en"));
        assert!(rendered.contains("PATH (6 entries)"));
        assert!(rendered.contains("... +1 more"));
        assert!(rendered.contains("RUST_LOG=debug"));
    }

    #[test]
    fn env_filter_includes_other_matching_vars() {
        let vars = vec![("CUSTOM_TOKEN".to_string(), "abcde".to_string())];
        let rendered = render_env_vars(&vars, Some("custom"), false);
        assert!(rendered.contains("CUSTOM_TOKEN=ab****de"));
        assert!(!rendered.contains("Total:"));
    }

    #[test]
    fn deps_summarizes_multiple_manifest_types() {
        let dir = tempfile::TempDir::new().unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[dependencies]\nanyhow = \"1\"\nserde = { version = \"1\" }\n[dev-dependencies]\ntempfile = \"3\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("package.json"),
            r#"{"name":"demo","version":"0.1.0","dependencies":{"react":"18"},"devDependencies":{"vite":"5"}}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("requirements.txt"),
            "pytest==8\n# ignored\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("go.mod"),
            "module example.com/demo\ngo 1.22\nrequire github.com/acme/lib v1.2.3\n",
        )
        .unwrap();
        let rendered = summarize_deps_dir(dir.path()).unwrap();
        assert!(rendered.contains("Rust (Cargo.toml):"));
        assert!(rendered.contains("anyhow (1)"));
        assert!(rendered.contains("Node.js (package.json):"));
        assert!(rendered.contains("demo @ 0.1.0"));
        assert!(rendered.contains("pytest==8"));
        assert!(rendered.contains("example.com/demo (go 1.22)"));
    }

    #[test]
    fn log_analysis_normalizes_variable_ids_and_paths() {
        let rendered = analyze_logs(
            "2026-05-12T01:00:00 ERROR failed request 1001 at /tmp/a\n\
             2026-05-12T01:00:01 ERROR failed request 1002 at /tmp/b\n\
             2026-05-12T01:00:02 WARN retry 2001\n\
             2026-05-12T01:00:03 INFO healthy\n",
        )
        .unwrap();
        assert!(rendered.contains("[error] 2 errors (1 unique)"));
        assert!(rendered.contains("[warn] 1 warnings (1 unique)"));
        assert!(rendered.contains("[info] 1 info messages"));
        assert!(rendered.contains("[x2]"));
    }
}
