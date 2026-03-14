use crate::types::{CommandReducerSpec, CommandReduction};

pub fn classify_git_command(command: &str, argv: &[String]) -> Option<CommandReducerSpec> {
    let subcommand = argv.get(1)?.as_str();
    let (canonical_kind, mutation) = match subcommand {
        "status" => ("git_status", false),
        "log" if !contains_any(argv, &["--format", "--pretty", "-p", "--patch", "--raw"]) => {
            ("git_log", false)
        }
        "diff" if !contains_any(argv, &["-p", "--patch", "--raw", "--word-diff"]) => {
            ("git_diff", false)
        }
        "add" => ("git_add", true),
        "commit" => ("git_commit", true),
        "push" => ("git_push", false),
        "pull" => ("git_pull", true),
        "branch" => ("git_branch", false),
        "switch" => ("git_switch", true),
        "checkout" => ("git_checkout", true),
        _ => return None,
    };
    let paths = argv
        .iter()
        .skip(2)
        .filter(|arg| !arg.starts_with('-'))
        .cloned()
        .collect::<Vec<_>>();
    Some(CommandReducerSpec {
        family: "git".to_string(),
        canonical_kind: canonical_kind.to_string(),
        packet_type: "packet28.hook.git.v2".to_string(),
        operation_kind: suite_packet_core::ToolOperationKind::Git,
        command: command.to_string(),
        argv: argv.to_vec(),
        cache_fingerprint: fingerprint("git", canonical_kind, argv),
        cacheable: !mutation,
        mutation,
        paths: normalize_paths(paths),
        equivalence_key: None,
    })
}

pub fn reduce_git_command(
    spec: &CommandReducerSpec,
    stdout: &str,
    stderr: &str,
    exit_code: i32,
) -> CommandReduction {
    let failed = exit_code != 0;
    let output = first_nonempty_line(stdout).or_else(|| first_nonempty_line(stderr));
    let summary = match spec.canonical_kind.as_str() {
        "git_status" => {
            let changed = stdout
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count();
            if failed {
                output.unwrap_or_else(|| "git status failed".to_string())
            } else {
                format!(
                    "git status reported {changed} changed entr{suffix}",
                    suffix = if changed == 1 { "y" } else { "ies" }
                )
            }
        }
        "git_log" => {
            let commits = stdout
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count();
            if failed {
                output.unwrap_or_else(|| "git log failed".to_string())
            } else {
                format!(
                    "git log returned {commits} commit entr{suffix}",
                    suffix = if commits == 1 { "y" } else { "ies" }
                )
            }
        }
        "git_diff" => {
            let files = stdout
                .lines()
                .filter(|line| {
                    line.starts_with("diff --git ")
                        || line.starts_with("--- ")
                        || line.starts_with("+++ ")
                })
                .count();
            if failed {
                output.unwrap_or_else(|| "git diff failed".to_string())
            } else if files > 0 {
                format!("git diff produced {files} diff marker(s)")
            } else {
                first_nonempty_line(stdout).unwrap_or_else(|| "git diff completed".to_string())
            }
        }
        _ => output.unwrap_or_else(|| format!("{} completed", spec.canonical_kind)),
    };
    CommandReduction {
        family: spec.family.clone(),
        canonical_kind: spec.canonical_kind.clone(),
        packet_type: spec.packet_type.clone(),
        operation_kind: spec.operation_kind,
        summary,
        paths: spec.paths.clone(),
        regions: Vec::new(),
        symbols: Vec::new(),
        failed,
        error_class: failed.then(|| "git_error".to_string()),
        error_message: failed.then(|| compact(stderr, 200)),
        retryable: failed.then_some(false),
        exit_code,
        cache_fingerprint: spec.cache_fingerprint.clone(),
        cacheable: spec.cacheable,
        mutation: spec.mutation,
        equivalence_key: spec.equivalence_key.clone(),
    }
}

fn contains_any(argv: &[String], denied: &[&str]) -> bool {
    argv.iter().any(|arg| {
        denied
            .iter()
            .any(|denied| arg == denied || arg.strip_prefix(&format!("{denied}=")).is_some())
    })
}

fn normalize_paths(paths: Vec<String>) -> Vec<String> {
    paths.into_iter().filter(|path| !path.is_empty()).collect()
}

fn fingerprint(family: &str, kind: &str, argv: &[String]) -> String {
    format!("{family}:{kind}:{}", argv.join("\u{1f}"))
}

fn first_nonempty_line(value: &str) -> Option<String> {
    value
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToOwned::to_owned)
}

fn compact(value: &str, limit: usize) -> String {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.len() <= limit {
        compact
    } else {
        format!("{}...", &compact[..limit.saturating_sub(3)])
    }
}
