use anyhow::Result;

use crate::fs::{classify_fs_command, reduce_fs_command};
use crate::git::{classify_git_command, reduce_git_command};
use crate::github::{classify_github_command, reduce_github_command};
use crate::go::{classify_go_command, reduce_go_command};
use crate::infra::{classify_infra_command, reduce_infra_command};
use crate::javascript::{classify_javascript_command, reduce_javascript_command};
use crate::python::{classify_python_command, reduce_python_command};
use crate::rust::{classify_rust_command, reduce_rust_command};
use crate::types::{CommandReducerFamily, CommandReducerSpec, CommandReduction};

pub fn classify_command(command: &str) -> Option<CommandReducerSpec> {
    let trimmed = command.trim();
    if trimmed.is_empty() || contains_shell_composition(trimmed) || contains_shell_expansion(trimmed)
    {
        return None;
    }
    let argv = shell_words::split(trimmed).ok()?;
    if argv.is_empty() || looks_like_env_assignment(&argv[0]) {
        return None;
    }
    classify_command_argv(trimmed, &argv)
}

pub fn classify_command_argv(command: &str, argv: &[String]) -> Option<CommandReducerSpec> {
    match argv.first()?.as_str() {
        "git" => classify_git_command(command, argv),
        "ls" | "find" | "cat" | "head" | "tail" | "sed" | "diff" => {
            classify_fs_command(command, argv)
        }
        "cargo" => classify_rust_command(command, argv),
        "gh" => classify_github_command(command, argv),
        "go" | "golangci-lint" => classify_go_command(command, argv),
        "docker" | "kubectl" | "curl" => classify_infra_command(command, argv),
        "python" | "python3" | "pytest" | "ruff" => classify_python_command(command, argv),
        "npm" | "pnpm" | "yarn" | "npx" | "tsc" | "eslint" | "vitest" => {
            classify_javascript_command(command, argv)
        }
        _ => None,
    }
}

pub fn reduce_command_output(
    spec: &CommandReducerSpec,
    stdout: &str,
    stderr: &str,
    exit_code: i32,
) -> Result<CommandReduction> {
    let family = parse_family(&spec.family)?;
    Ok(match family {
        CommandReducerFamily::Git => reduce_git_command(spec, stdout, stderr, exit_code),
        CommandReducerFamily::Fs => reduce_fs_command(spec, stdout, stderr, exit_code),
        CommandReducerFamily::Rust => reduce_rust_command(spec, stdout, stderr, exit_code),
        CommandReducerFamily::Github => reduce_github_command(spec, stdout, stderr, exit_code),
        CommandReducerFamily::Go => reduce_go_command(spec, stdout, stderr, exit_code),
        CommandReducerFamily::Infra => reduce_infra_command(spec, stdout, stderr, exit_code),
        CommandReducerFamily::Python => reduce_python_command(spec, stdout, stderr, exit_code),
        CommandReducerFamily::Javascript => {
            reduce_javascript_command(spec, stdout, stderr, exit_code)
        }
    })
}

fn parse_family(value: &str) -> Result<CommandReducerFamily> {
    Ok(match value {
        "git" => CommandReducerFamily::Git,
        "fs" => CommandReducerFamily::Fs,
        "rust" => CommandReducerFamily::Rust,
        "github" => CommandReducerFamily::Github,
        "go" => CommandReducerFamily::Go,
        "infra" => CommandReducerFamily::Infra,
        "python" => CommandReducerFamily::Python,
        "javascript" => CommandReducerFamily::Javascript,
        other => anyhow::bail!("unsupported reducer family '{other}'"),
    })
}

fn looks_like_env_assignment(arg: &str) -> bool {
    arg.contains('=')
        && !arg.starts_with('=')
        && arg
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_')
}

fn contains_shell_composition(command: &str) -> bool {
    let bytes = command.as_bytes();
    let mut in_single = false;
    let mut in_double = false;
    let mut idx = 0;
    while idx < bytes.len() {
        let ch = bytes[idx] as char;
        if ch == '\'' && !in_double {
            in_single = !in_single;
        } else if ch == '"' && !in_single {
            in_double = !in_double;
        } else if !in_single && !in_double {
            if matches!(ch, '|' | ';' | '<' | '>' | '`') {
                return true;
            }
            if ch == '&' {
                return true;
            }
            if ch == '$' && bytes.get(idx + 1).is_some_and(|next| *next == b'(') {
                return true;
            }
        }
        idx += 1;
    }
    false
}

fn contains_shell_expansion(command: &str) -> bool {
    let bytes = command.as_bytes();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut idx = 0;
    while idx < bytes.len() {
        let ch = bytes[idx] as char;
        if escaped {
            escaped = false;
            idx += 1;
            continue;
        }
        match ch {
            '\\' if !in_single => {
                escaped = true;
            }
            '\'' if !in_double => {
                in_single = !in_single;
            }
            '"' if !in_single => {
                in_double = !in_double;
            }
            '$' if !in_single => {
                return true;
            }
            '~' if !in_single && !in_double && idx == 0 => {
                return true;
            }
            '*' | '?' if !in_single && !in_double => {
                return true;
            }
            '[' | '{' if !in_single && !in_double => {
                return true;
            }
            _ => {}
        }
        idx += 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declines_shell_composition() {
        assert!(classify_command("cargo test 2>&1 | grep FAIL").is_none());
        assert!(classify_command("FOO=1 cargo test").is_none());
    }

    #[test]
    fn declines_shell_expansion() {
        assert!(classify_command("git diff src/*.rs").is_none());
        assert!(classify_command("cargo test $CRATE").is_none());
        assert!(classify_command("~/bin/tool").is_none());
    }
}
