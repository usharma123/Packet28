//! Learn module: detect error → correction patterns in session JSONL.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;
use serde::Serialize;
use serde_json::Value;

#[derive(Args, Clone)]
pub struct LearnArgs {
    /// Path to Claude projects directory
    #[arg(long)]
    pub sessions_dir: Option<String>,

    /// Maximum sessions to scan
    #[arg(long, default_value_t = 20)]
    pub limit: usize,

    /// Minimum frequency to include a correction
    #[arg(long, default_value_t = 2)]
    pub min_frequency: usize,

    /// Output as JSON
    #[arg(long)]
    pub json: bool,

    /// Pretty-print JSON
    #[arg(long)]
    pub pretty: bool,

    /// Write corrections to .claude/rules/cli-corrections.md
    #[arg(long)]
    pub write_rules: bool,
}

#[derive(Debug, Serialize, Default)]
struct LearnReport {
    sessions_scanned: usize,
    corrections_found: usize,
    corrections: Vec<Correction>,
}

#[derive(Debug, Serialize, Clone)]
struct Correction {
    failed_command: String,
    successful_command: String,
    frequency: usize,
    confidence: f64,
}

pub fn run(args: LearnArgs) -> Result<i32> {
    let sessions_dir = args
        .sessions_dir
        .map(PathBuf::from)
        .unwrap_or_else(default_sessions_dir);

    let mut report = LearnReport::default();

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

    // Collect all corrections across sessions
    let mut correction_counts: BTreeMap<(String, String), usize> = BTreeMap::new();

    for file in &session_files {
        if let Ok(corrections) = extract_corrections(file) {
            for (failed, success) in corrections {
                *correction_counts.entry((failed, success)).or_insert(0) += 1;
            }
        }
    }

    // Filter by minimum frequency and compute confidence
    let total_corrections: usize = correction_counts.values().sum();
    for ((failed, success), count) in &correction_counts {
        if *count >= args.min_frequency {
            let confidence = (*count as f64) / (total_corrections.max(1) as f64);
            report.corrections.push(Correction {
                failed_command: failed.clone(),
                successful_command: success.clone(),
                frequency: *count,
                confidence,
            });
        }
    }

    report
        .corrections
        .sort_by(|a, b| b.frequency.cmp(&a.frequency));
    report.corrections_found = report.corrections.len();

    if args.write_rules && !report.corrections.is_empty() {
        write_corrections_rules(&report.corrections)?;
    }

    if args.json {
        crate::cmd_common::emit_json(&serde_json::to_value(&report)?, args.pretty)?;
    } else {
        println!("Sessions scanned: {}", report.sessions_scanned);
        println!("Corrections found: {}", report.corrections_found);
        for correction in report.corrections.iter().take(20) {
            println!(
                "  {} -> {} ({}x, confidence: {:.0}%)",
                correction.failed_command,
                correction.successful_command,
                correction.frequency,
                correction.confidence * 100.0
            );
        }
        if args.write_rules {
            println!("\nCorrections written to .claude/rules/cli-corrections.md");
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
    for entry in fs::read_dir(dir).with_context(|| format!("read dir {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
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
        }
        if files.len() >= limit * 5 {
            break;
        }
    }
    files.sort_by(|a, b| {
        let a_time = fs::metadata(a).and_then(|m| m.modified()).ok();
        let b_time = fs::metadata(b).and_then(|m| m.modified()).ok();
        b_time.cmp(&a_time)
    });
    files.truncate(limit);
    Ok(files)
}

fn extract_corrections(path: &Path) -> Result<Vec<(String, String)>> {
    let content = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;

    let mut bash_results: Vec<(String, bool)> = Vec::new();

    for line in content.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };

        // Look for tool results from Bash commands
        if value.get("type").and_then(Value::as_str) == Some("user") {
            if let Some(content) = value
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_array)
            {
                for block in content {
                    if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                        let is_error = block
                            .get("is_error")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        let result_content =
                            block.get("content").and_then(Value::as_str).unwrap_or("");
                        // Try to extract the command from the context
                        if let Some(cmd) = extract_command_from_context(&value) {
                            bash_results.push((
                                cmd,
                                is_error
                                    || result_content.contains("error")
                                    || result_content.contains("Error"),
                            ));
                        }
                    }
                }
            }
        }

        // Also look for assistant tool_use with Bash
        if value.get("type").and_then(Value::as_str) == Some("assistant") {
            if let Some(content) = value
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_array)
            {
                for block in content {
                    if block.get("type").and_then(Value::as_str) == Some("tool_use")
                        && block.get("name").and_then(Value::as_str) == Some("Bash")
                    {
                        if let Some(cmd) = block
                            .get("input")
                            .and_then(|i| i.get("command"))
                            .and_then(Value::as_str)
                        {
                            // This will be matched with its result later
                            bash_results.push((cmd.to_string(), false));
                        }
                    }
                }
            }
        }
    }

    // Find fail → success correction patterns
    let mut corrections = Vec::new();
    for window in bash_results.windows(2) {
        let (ref cmd1, failed1) = window[0];
        let (ref cmd2, failed2) = window[1];
        if failed1 && !failed2 {
            // Extract the base command to check if they're related
            let base1 = cmd1.split_whitespace().next().unwrap_or("");
            let base2 = cmd2.split_whitespace().next().unwrap_or("");
            if base1 == base2 {
                let short1 = truncate_command(cmd1, 80);
                let short2 = truncate_command(cmd2, 80);
                corrections.push((short1, short2));
            }
        }
    }

    Ok(corrections)
}

fn extract_command_from_context(value: &Value) -> Option<String> {
    // Try to find the command from the tool_use_id context
    value
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)?
        .iter()
        .find_map(|block| {
            if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                block
                    .get("content")
                    .and_then(Value::as_str)
                    .map(|c| c.lines().next().unwrap_or("").to_string())
            } else {
                None
            }
        })
}

fn truncate_command(cmd: &str, max: usize) -> String {
    if cmd.len() <= max {
        cmd.to_string()
    } else {
        format!("{}...", &cmd[..max.saturating_sub(3)])
    }
}

fn write_corrections_rules(corrections: &[Correction]) -> Result<()> {
    let dir = PathBuf::from(".claude/rules");
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;

    let path = dir.join("cli-corrections.md");
    let mut content =
        String::from("# CLI Corrections (auto-generated by `covy compact learn`)\n\n");
    content.push_str("These patterns were learned from session history.\n\n");

    for correction in corrections.iter().take(50) {
        content.push_str(&format!(
            "- Instead of `{}`, prefer `{}` (seen {}x)\n",
            correction.failed_command, correction.successful_command, correction.frequency
        ));
    }

    fs::write(&path, &content).with_context(|| format!("failed to write {}", path.display()))?;

    Ok(())
}
