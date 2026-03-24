//! Discover module: scan Claude session JSONL for command patterns and savings opportunities.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;
use serde::Serialize;
use serde_json::Value;

#[derive(Args, Clone)]
pub struct DiscoverArgs {
    /// Path to Claude projects directory
    #[arg(long)]
    pub sessions_dir: Option<String>,

    /// Maximum sessions to scan
    #[arg(long, default_value_t = 20)]
    pub limit: usize,

    /// Output as JSON
    #[arg(long)]
    pub json: bool,

    /// Pretty-print JSON
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Debug, Serialize, Default)]
struct DiscoverReport {
    sessions_scanned: usize,
    commands_found: usize,
    supported_commands: usize,
    unsupported_commands: usize,
    by_category: BTreeMap<String, CategoryStats>,
    top_unsupported: Vec<UnsupportedCommand>,
}

#[derive(Debug, Serialize, Default)]
struct CategoryStats {
    count: usize,
    estimated_tokens: u64,
}

#[derive(Debug, Serialize)]
struct UnsupportedCommand {
    command: String,
    count: usize,
    estimated_tokens: u64,
}

pub fn run(args: DiscoverArgs) -> Result<i32> {
    let sessions_dir = args
        .sessions_dir
        .map(PathBuf::from)
        .unwrap_or_else(default_sessions_dir);

    let mut report = DiscoverReport::default();

    if !sessions_dir.exists() {
        if args.json {
            crate::cmd_common::emit_json(&serde_json::to_value(&report)?, args.pretty)?;
        } else {
            println!("No sessions directory found at {}", sessions_dir.display());
        }
        return Ok(0);
    }

    let session_files = collect_session_files(&sessions_dir, args.limit)?;
    report.sessions_scanned = session_files.len();

    let mut command_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut command_tokens: BTreeMap<String, u64> = BTreeMap::new();

    for file in &session_files {
        if let Ok(commands) = extract_bash_commands(file) {
            for (cmd, est_tokens) in commands {
                report.commands_found += 1;
                let program = cmd.split_whitespace().next().unwrap_or("").to_string();
                *command_counts.entry(program.clone()).or_insert(0) += 1;
                *command_tokens.entry(program).or_insert(0) += est_tokens;
            }
        }
    }

    for (program, count) in &command_counts {
        let tokens = command_tokens.get(program).copied().unwrap_or(0);
        let classified = packet28_reducer_core::classify_command(&format!("{program} --help"))
            .is_some()
            || is_known_reducible(program);

        if classified {
            report.supported_commands += count;
            let category = categorize_command(program);
            let entry = report.by_category.entry(category).or_default();
            entry.count += count;
            entry.estimated_tokens += tokens;
        } else {
            report.unsupported_commands += count;
            report.top_unsupported.push(UnsupportedCommand {
                command: program.clone(),
                count: *count,
                estimated_tokens: tokens,
            });
        }
    }

    report
        .top_unsupported
        .sort_by(|a, b| b.estimated_tokens.cmp(&a.estimated_tokens));
    report.top_unsupported.truncate(20);

    if args.json {
        crate::cmd_common::emit_json(&serde_json::to_value(&report)?, args.pretty)?;
    } else {
        println!("Sessions scanned: {}", report.sessions_scanned);
        println!("Commands found: {}", report.commands_found);
        println!("Supported: {}", report.supported_commands);
        println!("Unsupported: {}", report.unsupported_commands);
        if !report.by_category.is_empty() {
            println!("\nBy category:");
            for (category, stats) in &report.by_category {
                println!(
                    "  {category}: {} commands, ~{} tokens",
                    stats.count,
                    crate::economics::format_tokens(stats.estimated_tokens)
                );
            }
        }
        if !report.top_unsupported.is_empty() {
            println!("\nTop unsupported commands:");
            for cmd in report.top_unsupported.iter().take(10) {
                println!(
                    "  {}: {}x (~{} tokens)",
                    cmd.command,
                    cmd.count,
                    crate::economics::format_tokens(cmd.estimated_tokens)
                );
            }
        }
    }

    Ok(0)
}

fn default_sessions_dir() -> PathBuf {
    #[cfg(unix)]
    {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(format!("{home}/.claude/projects"));
        }
    }
    PathBuf::from("/tmp/.claude/projects")
}

fn collect_session_files(dir: &Path, limit: usize) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    if !dir.is_dir() {
        return Ok(files);
    }
    // Walk project directories looking for session JSONL files
    for entry in fs::read_dir(dir).with_context(|| format!("read dir {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            // Look for sessions subdirectory or JSONL files directly
            let sessions_subdir = path.join("sessions");
            let scan_dir = if sessions_subdir.is_dir() {
                sessions_subdir
            } else {
                path
            };
            if let Ok(entries) = fs::read_dir(&scan_dir) {
                for sub_entry in entries.flatten() {
                    let sub_path = sub_entry.path();
                    if sub_path.extension().is_some_and(|ext| ext == "jsonl") {
                        files.push(sub_path);
                    }
                }
            }
        } else if path.extension().is_some_and(|ext| ext == "jsonl") {
            files.push(path);
        }
        if files.len() >= limit * 5 {
            break;
        }
    }
    // Sort by modification time, newest first
    files.sort_by(|a, b| {
        let a_time = fs::metadata(a).and_then(|m| m.modified()).ok();
        let b_time = fs::metadata(b).and_then(|m| m.modified()).ok();
        b_time.cmp(&a_time)
    });
    files.truncate(limit);
    Ok(files)
}

fn extract_bash_commands(path: &Path) -> Result<Vec<(String, u64)>> {
    let mut commands = Vec::new();
    let content = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;

    for line in content.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };

        // Look for assistant messages with tool_use content
        if value.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(content) = value.get("message").and_then(|m| m.get("content")) else {
            continue;
        };
        let Some(blocks) = content.as_array() else {
            continue;
        };

        for block in blocks {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let tool_name = block.get("name").and_then(Value::as_str).unwrap_or("");
            if tool_name != "Bash" {
                continue;
            }
            if let Some(command) = block
                .get("input")
                .and_then(|input| input.get("command"))
                .and_then(Value::as_str)
            {
                let est_tokens = (command.len() as u64) / 4;
                commands.push((command.to_string(), est_tokens));
            }
        }
    }

    Ok(commands)
}

fn is_known_reducible(program: &str) -> bool {
    matches!(
        program,
        "git"
            | "cargo"
            | "gh"
            | "go"
            | "golangci-lint"
            | "docker"
            | "kubectl"
            | "curl"
            | "python"
            | "python3"
            | "pytest"
            | "ruff"
            | "mypy"
            | "pip"
            | "pip3"
            | "uv"
            | "npm"
            | "pnpm"
            | "yarn"
            | "npx"
            | "tsc"
            | "eslint"
            | "vitest"
            | "prettier"
            | "next"
            | "prisma"
            | "playwright"
            | "ls"
            | "find"
            | "cat"
            | "head"
            | "tail"
            | "sed"
            | "diff"
            | "aws"
    )
}

fn categorize_command(program: &str) -> String {
    match program {
        "git" => "git",
        "cargo" => "rust",
        "gh" => "github",
        "go" | "golangci-lint" => "go",
        "docker" | "kubectl" | "curl" | "aws" => "infra",
        "python" | "python3" | "pytest" | "ruff" | "mypy" | "pip" | "pip3" | "uv" => "python",
        "npm" | "pnpm" | "yarn" | "npx" | "tsc" | "eslint" | "vitest" | "prettier" | "next"
        | "prisma" | "playwright" => "javascript",
        "ls" | "find" | "cat" | "head" | "tail" | "sed" | "diff" => "fs",
        _ => "other",
    }
    .to_string()
}
