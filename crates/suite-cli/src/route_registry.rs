use std::path::Path;

use packet28_reducer_core::{classify_command_argv, CommandReducerSpec};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteKind {
    ReducerRewrite,
    NativeTool,
    ProxyPassthrough,
    RawPassthrough,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeToolKind {
    Tree,
    Read,
    Grep,
    Env,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeToolPlan {
    pub kind: NativeToolKind,
    pub argv: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RouteDecision {
    pub kind: RouteKind,
    pub reason: Option<String>,
    pub argv: Vec<String>,
    pub env_assignments: Vec<(String, String)>,
    pub reducer_spec: Option<CommandReducerSpec>,
    pub native_tool: Option<NativeToolPlan>,
}

pub fn decide_command_route(command: &str) -> RouteDecision {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return RouteDecision {
            kind: RouteKind::RawPassthrough,
            reason: Some("empty_command".to_string()),
            argv: Vec::new(),
            env_assignments: Vec::new(),
            reducer_spec: None,
            native_tool: None,
        };
    }

    if contains_shell_composition(trimmed) {
        return raw_passthrough("shell_composition");
    }
    if contains_disallowed_shell_expansion(trimmed) {
        return raw_passthrough("shell_expansion");
    }

    let Ok(argv) = shell_words::split(trimmed) else {
        return raw_passthrough("shell_parse_error");
    };
    let (env_assignments, argv) = split_leading_env_assignments(argv);
    if argv.is_empty() {
        return raw_passthrough("empty_command");
    }

    let normalized = shell_join(&argv);
    if let Some(spec) = classify_command_argv(&normalized, &argv) {
        return RouteDecision {
            kind: RouteKind::ReducerRewrite,
            reason: None,
            argv,
            env_assignments,
            reducer_spec: Some(spec),
            native_tool: None,
        };
    }

    if env_assignments.is_empty() {
        if let Some(native_tool) = classify_native_tool(&argv) {
            return RouteDecision {
                kind: RouteKind::NativeTool,
                reason: None,
                argv,
                env_assignments,
                reducer_spec: None,
                native_tool: Some(native_tool),
            };
        }
    }

    if suite_proxy_core::command_supported(&argv) {
        return RouteDecision {
            kind: RouteKind::ProxyPassthrough,
            reason: Some("proxy_summary".to_string()),
            argv,
            env_assignments,
            reducer_spec: None,
            native_tool: None,
        };
    }

    raw_passthrough("unsupported_command")
}

pub fn build_route_rewrite(
    root: &Path,
    task_id: &str,
    session_id: Option<&str>,
    cwd: &str,
    decision: &RouteDecision,
) -> Option<String> {
    match decision.kind {
        RouteKind::ReducerRewrite => {
            let spec = decision.reducer_spec.as_ref()?;
            Some(build_reducer_rewrite(
                root,
                task_id,
                session_id,
                cwd,
                spec,
                &decision.argv,
                &decision.env_assignments,
            ))
        }
        RouteKind::NativeTool => {
            let native_tool = decision.native_tool.as_ref()?;
            Some(build_native_tool_rewrite(root, task_id, cwd, native_tool))
        }
        RouteKind::ProxyPassthrough => Some(build_proxy_rewrite(root, task_id, cwd, &decision.argv)),
        RouteKind::RawPassthrough => None,
    }
}

fn raw_passthrough(reason: &str) -> RouteDecision {
    RouteDecision {
        kind: RouteKind::RawPassthrough,
        reason: Some(reason.to_string()),
        argv: Vec::new(),
        env_assignments: Vec::new(),
        reducer_spec: None,
        native_tool: None,
    }
}

fn split_leading_env_assignments(argv: Vec<String>) -> (Vec<(String, String)>, Vec<String>) {
    let mut assignments = Vec::new();
    let mut idx = 0usize;
    while idx < argv.len() && looks_like_env_assignment(&argv[idx]) {
        let mut parts = argv[idx].splitn(2, '=');
        let key = parts.next().unwrap_or_default().trim().to_string();
        let value = parts.next().unwrap_or_default().to_string();
        assignments.push((key, value));
        idx += 1;
    }
    (assignments, argv[idx..].to_vec())
}

fn classify_native_tool(argv: &[String]) -> Option<NativeToolPlan> {
    match argv.first()?.as_str() {
        "ls" | "find" => classify_tree_tool(argv),
        "cat" | "head" | "tail" | "sed" => classify_read_tool(argv),
        "grep" | "rg" => classify_grep_tool(argv),
        "env" | "printenv" => classify_env_tool(argv),
        _ => None,
    }
}

fn classify_tree_tool(argv: &[String]) -> Option<NativeToolPlan> {
    let mut tool_argv = vec!["compact".to_string(), "tree".to_string()];
    let mut paths = Vec::<String>::new();
    let mut hidden = false;
    let mut max_depth = None::<String>;
    match argv.first()?.as_str() {
        "ls" => {
            for arg in argv.iter().skip(1) {
                if arg.starts_with('-') {
                    if arg.contains('a') {
                        hidden = true;
                    }
                    if arg.contains('R') {
                        max_depth = Some("8".to_string());
                    }
                    continue;
                }
                paths.push(arg.clone());
            }
        }
        "find" => {
            let mut iter = argv.iter().skip(1);
            while let Some(arg) = iter.next() {
                match arg.as_str() {
                    "-maxdepth" => {
                        if let Some(value) = iter.next() {
                            max_depth = Some(value.clone());
                        }
                    }
                    "-name" | "-type" | "-path" => {
                        let _ = iter.next();
                    }
                    value if value.starts_with('-') => {}
                    value => paths.push(value.to_string()),
                }
            }
        }
        _ => return None,
    }
    if hidden {
        tool_argv.push("--hidden".to_string());
    }
    if let Some(max_depth) = max_depth {
        tool_argv.push("--max-depth".to_string());
        tool_argv.push(max_depth);
    }
    if paths.is_empty() {
        tool_argv.push(".".to_string());
    } else {
        tool_argv.extend(paths);
    }
    Some(NativeToolPlan {
        kind: NativeToolKind::Tree,
        argv: tool_argv,
    })
}

fn classify_read_tool(argv: &[String]) -> Option<NativeToolPlan> {
    let mut tool_argv = vec!["compact".to_string(), "read".to_string()];
    match argv.first()?.as_str() {
        "cat" => {
            let path = argv.get(1)?.clone();
            tool_argv.push(path);
        }
        "head" => {
            let (count, path) = parse_line_count_and_path(argv.iter().skip(1).cloned().collect())?;
            tool_argv.push("--line-start".to_string());
            tool_argv.push("1".to_string());
            tool_argv.push("--line-end".to_string());
            tool_argv.push(count.to_string());
            tool_argv.push(path);
        }
        "tail" => {
            let (count, path) = parse_line_count_and_path(argv.iter().skip(1).cloned().collect())?;
            tool_argv.push("--last".to_string());
            tool_argv.push(count.to_string());
            tool_argv.push(path);
        }
        "sed" => {
            if argv.len() < 4 || argv.get(1).map(String::as_str) != Some("-n") {
                return None;
            }
            let range = argv.get(2)?;
            let path = argv.get(3)?.clone();
            let (start, end) = parse_sed_range(range)?;
            tool_argv.push("--line-start".to_string());
            tool_argv.push(start.to_string());
            tool_argv.push("--line-end".to_string());
            tool_argv.push(end.to_string());
            tool_argv.push(path);
        }
        _ => return None,
    }
    Some(NativeToolPlan {
        kind: NativeToolKind::Read,
        argv: tool_argv,
    })
}

fn classify_grep_tool(argv: &[String]) -> Option<NativeToolPlan> {
    let mut tool_argv = vec!["compact".to_string(), "grep".to_string()];
    let mut fixed_string = false;
    let mut case_insensitive = false;
    let mut whole_word = false;
    let mut query = None::<String>;
    let mut paths = Vec::<String>::new();
    let mut iter = argv.iter().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-F" => fixed_string = true,
            "-i" => case_insensitive = true,
            "-w" => whole_word = true,
            "-e" => query = iter.next().cloned(),
            value if value.starts_with('-') => {}
            value => {
                if query.is_none() {
                    query = Some(value.to_string());
                } else {
                    paths.push(value.to_string());
                }
            }
        }
    }
    let query = query?;
    if fixed_string {
        tool_argv.push("--fixed-string".to_string());
    }
    if case_insensitive {
        tool_argv.push("--ignore-case".to_string());
    }
    if whole_word {
        tool_argv.push("--whole-word".to_string());
    }
    tool_argv.push(query);
    if paths.is_empty() {
        tool_argv.push(".".to_string());
    } else {
        tool_argv.extend(paths);
    }
    Some(NativeToolPlan {
        kind: NativeToolKind::Grep,
        argv: tool_argv,
    })
}

fn classify_env_tool(argv: &[String]) -> Option<NativeToolPlan> {
    let mut tool_argv = vec!["compact".to_string(), "env".to_string()];
    if let Some(prefix) = argv.get(1).filter(|value| !value.starts_with('-')) {
        tool_argv.push("--prefix".to_string());
        tool_argv.push(prefix.clone());
    }
    Some(NativeToolPlan {
        kind: NativeToolKind::Env,
        argv: tool_argv,
    })
}

fn parse_line_count_and_path(argv: Vec<String>) -> Option<(usize, String)> {
    let mut count = 10usize;
    let mut path = None::<String>;
    let mut iter = argv.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-n" => {
                count = iter.next()?.parse().ok()?;
            }
            value if value.starts_with('-') => {}
            value => path = Some(value.to_string()),
        }
    }
    Some((count, path?))
}

fn parse_sed_range(value: &str) -> Option<(usize, usize)> {
    let trimmed = value.trim().trim_end_matches('p');
    let (start, end) = trimmed.split_once(',')?;
    Some((start.parse().ok()?, end.parse().ok()?))
}

fn build_reducer_rewrite(
    root: &Path,
    task_id: &str,
    session_id: Option<&str>,
    cwd: &str,
    spec: &CommandReducerSpec,
    argv: &[String],
    env_assignments: &[(String, String)],
) -> String {
    let exe = current_exe();
    let mut parts = vec![
        shell_quote(&exe),
        "hook".to_string(),
        "reducer-runner".to_string(),
        "--root".to_string(),
        shell_quote(&root.display().to_string()),
        "--task-id".to_string(),
        shell_quote(task_id),
        "--family".to_string(),
        shell_quote(&spec.family),
        "--kind".to_string(),
        shell_quote(&spec.canonical_kind),
        "--fingerprint".to_string(),
        shell_quote(&spec.cache_fingerprint),
    ];
    if let Some(session_id) = session_id.filter(|value| !value.trim().is_empty()) {
        parts.push("--session-id".to_string());
        parts.push(shell_quote(session_id));
    }
    for (key, value) in env_assignments {
        parts.push("--env".to_string());
        parts.push(shell_quote(&format!("{key}={value}")));
    }
    parts.push("--cwd".to_string());
    parts.push(shell_quote(cwd));
    parts.push("--".to_string());
    parts.extend(argv.iter().map(|arg| shell_quote(arg)));
    parts.join(" ")
}

fn build_native_tool_rewrite(root: &Path, task_id: &str, cwd: &str, plan: &NativeToolPlan) -> String {
    let exe = current_exe();
    let mut parts = vec![
        shell_quote(&exe),
        "--via-daemon".to_string(),
        "--daemon-root".to_string(),
        shell_quote(&root.display().to_string()),
    ];
    let (prefix, suffix) = plan.argv.split_at(plan.argv.len().min(2));
    parts.extend(prefix.iter().map(|arg| shell_quote(arg)));
    parts.push("--root".to_string());
    parts.push(shell_quote(&root.display().to_string()));
    parts.push("--task-id".to_string());
    parts.push(shell_quote(task_id));
    parts.push("--cwd".to_string());
    parts.push(shell_quote(cwd));
    parts.extend(suffix.iter().map(|arg| shell_quote(arg)));
    parts.join(" ")
}

fn build_proxy_rewrite(root: &Path, task_id: &str, cwd: &str, argv: &[String]) -> String {
    let exe = current_exe();
    let mut parts = vec![
        shell_quote(&exe),
        "--via-daemon".to_string(),
        "--daemon-root".to_string(),
        shell_quote(&root.display().to_string()),
        "proxy".to_string(),
        "run".to_string(),
        "--task-id".to_string(),
        shell_quote(task_id),
        "--cwd".to_string(),
        shell_quote(cwd),
        "--".to_string(),
    ];
    parts.extend(argv.iter().map(|arg| shell_quote(arg)));
    parts.join(" ")
}

fn current_exe() -> String {
    std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "Packet28".to_string())
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
    let mut idx = 0usize;
    while idx < bytes.len() {
        let ch = bytes[idx] as char;
        if ch == '\'' && !in_double {
            in_single = !in_single;
        } else if ch == '"' && !in_single {
            in_double = !in_double;
        } else if !in_single && !in_double {
            if matches!(ch, '|' | ';' | '<' | '>' | '`' | '&') {
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

fn contains_disallowed_shell_expansion(command: &str) -> bool {
    let bytes = command.as_bytes();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut idx = 0usize;
    while idx < bytes.len() {
        let ch = bytes[idx] as char;
        if escaped {
            escaped = false;
            idx += 1;
            continue;
        }
        match ch {
            '\\' if !in_single => escaped = true,
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '$' if !in_single => return true,
            '~' if !in_single && !in_double && idx == 0 => return true,
            _ => {}
        }
        idx += 1;
    }
    false
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | ':' | '='))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_env_prefixed_reducer_commands() {
        let decision = decide_command_route("FOO=1 cargo test");
        assert_eq!(decision.kind, RouteKind::ReducerRewrite);
        assert_eq!(decision.env_assignments.len(), 1);
    }

    #[test]
    fn routes_simple_reads_to_native_tool() {
        let decision = decide_command_route("head -n 20 README.md");
        assert_eq!(decision.kind, RouteKind::NativeTool);
    }
}
