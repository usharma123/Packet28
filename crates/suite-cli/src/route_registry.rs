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
    /// Original argv before glob expansion, when expansion was applied.
    pub original_argv: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Postprocess {
    Head(usize),
    Tail(usize),
    SedRange(usize, usize),
}

pub fn decide_command_route(command: &str) -> RouteDecision {
    decide_command_route_inner(command, None)
}

pub fn decide_command_route_with_cwd(command: &str, cwd: &Path) -> RouteDecision {
    decide_command_route_inner(command, Some(cwd))
}

fn decide_command_route_inner(command: &str, cwd: Option<&Path>) -> RouteDecision {
    let sanitized = strip_supported_trailing_redirects(command.trim());
    let (normalized, postprocess) = strip_supported_postprocess(&sanitized);
    let trimmed = normalized.trim();
    if trimmed.is_empty() {
        return RouteDecision {
            kind: RouteKind::RawPassthrough,
            reason: Some("empty_command".to_string()),
            argv: Vec::new(),
            env_assignments: Vec::new(),
            reducer_spec: None,
            native_tool: None,
            original_argv: None,
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
    let (env_assignments, mut argv) = split_leading_env_assignments(argv);
    if argv.is_empty() {
        return raw_passthrough("empty_command");
    }

    let mut original_argv = None;
    if let Some(cwd) = cwd {
        if let Some(expanded) = try_expand_globs(&argv, cwd) {
            original_argv = Some(argv);
            argv = expanded;
        }
    }

    if env_assignments.is_empty() {
        if let Some(mut native_tool) = classify_native_tool(&argv) {
            if contains_unsupported_glob_tokens(&argv, Some(&native_tool)) {
                return raw_passthrough("shell_glob");
            }
            if !apply_postprocess_to_native_tool(&mut native_tool, postprocess.as_ref()) {
                return raw_passthrough("unsupported_postprocess");
            }
            return RouteDecision {
                kind: RouteKind::NativeTool,
                reason: None,
                argv,
                env_assignments,
                reducer_spec: None,
                native_tool: Some(native_tool),
                original_argv,
            };
        }
    }

    if contains_unsupported_glob_tokens(&argv, None) {
        return raw_passthrough("shell_glob");
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
            original_argv,
        };
    }

    if suite_proxy_core::command_supported(&argv) {
        return RouteDecision {
            kind: RouteKind::ProxyPassthrough,
            reason: Some("proxy_summary".to_string()),
            argv,
            env_assignments,
            reducer_spec: None,
            native_tool: None,
            original_argv,
        };
    }

    raw_passthrough("unsupported_command")
}

fn try_expand_globs(argv: &[String], cwd: &Path) -> Option<Vec<String>> {
    const MAX_MATCHES_PER_PATTERN: usize = 100;
    const MAX_TOTAL_ARGS: usize = 500;

    let mut expanded = Vec::with_capacity(argv.len());
    let mut total_args = 0usize;
    let mut any_expanded = false;

    for (i, arg) in argv.iter().enumerate() {
        if arg.starts_with('-') {
            expanded.push(arg.clone());
            total_args += 1;
            continue;
        }
        if i == 0 {
            expanded.push(arg.clone());
            total_args += 1;
            continue;
        }
        if i == 1 && !contains_glob_chars(arg) {
            expanded.push(arg.clone());
            total_args += 1;
            continue;
        }
        if !contains_glob_chars(arg) {
            expanded.push(arg.clone());
            total_args += 1;
            continue;
        }

        let pattern = if Path::new(arg).is_absolute() {
            arg.clone()
        } else {
            cwd.join(arg).display().to_string()
        };

        let mut matches: Vec<String> = match glob::glob(&pattern) {
            Ok(iter) => iter
                .filter_map(Result::ok)
                .take(MAX_MATCHES_PER_PATTERN)
                .map(|p| {
                    let s = p.display().to_string();
                    if Path::new(arg).is_absolute() {
                        s
                    } else if let Ok(rel) = p.strip_prefix(cwd) {
                        rel.display().to_string()
                    } else {
                        s
                    }
                })
                .collect(),
            Err(_) => {
                expanded.push(arg.clone());
                total_args += 1;
                continue;
            }
        };

        if matches.is_empty() {
            expanded.push(arg.clone());
            total_args += 1;
            continue;
        }

        matches.sort();
        matches.dedup();
        any_expanded = true;
        for m in matches {
            expanded.push(m);
            total_args += 1;
            if total_args > MAX_TOTAL_ARGS {
                return None;
            }
        }
    }

    if any_expanded {
        Some(expanded)
    } else {
        None
    }
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
        RouteKind::ProxyPassthrough => {
            Some(build_proxy_rewrite(root, task_id, cwd, &decision.argv))
        }
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
        original_argv: None,
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
        "ls" | "find" | "tree" => classify_tree_tool(argv),
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
        "tree" => {
            let mut iter = argv.iter().skip(1);
            while let Some(arg) = iter.next() {
                match arg.as_str() {
                    "-a" => hidden = true,
                    "-L" => {
                        if let Some(value) = iter.next() {
                            max_depth = Some(value.clone());
                        }
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

fn apply_postprocess_to_native_tool(
    plan: &mut NativeToolPlan,
    postprocess: Option<&Postprocess>,
) -> bool {
    let Some(postprocess) = postprocess else {
        return true;
    };
    match (&plan.kind, postprocess) {
        (NativeToolKind::Tree, Postprocess::Head(count)) => {
            upsert_tree_option(&mut plan.argv, "--max-entries", *count);
            true
        }
        (NativeToolKind::Grep, Postprocess::Head(count)) => {
            upsert_grep_option(&mut plan.argv, "--max-total-matches", *count);
            true
        }
        (NativeToolKind::Read, Postprocess::Head(count)) => {
            constrain_read_head(&mut plan.argv, *count)
        }
        (NativeToolKind::Read, Postprocess::Tail(count)) => {
            constrain_read_tail(&mut plan.argv, *count)
        }
        (NativeToolKind::Read, Postprocess::SedRange(start, end)) => {
            constrain_read_range(&mut plan.argv, *start, *end)
        }
        _ => false,
    }
}

fn constrain_read_head(argv: &mut Vec<String>, count: usize) -> bool {
    if option_value(argv, "--last").is_some() {
        return false;
    }
    let line_start = option_value(argv, "--line-start").unwrap_or(1);
    let existing_end = option_value(argv, "--line-end").unwrap_or(line_start + count - 1);
    let target_end = existing_end.min(line_start + count.saturating_sub(1));
    upsert_read_option(argv, "--line-start", line_start);
    upsert_read_option(argv, "--line-end", target_end);
    true
}

fn constrain_read_tail(argv: &mut Vec<String>, count: usize) -> bool {
    if option_value(argv, "--line-start").is_some() || option_value(argv, "--line-end").is_some() {
        return false;
    }
    let existing_last = option_value(argv, "--last").unwrap_or(count);
    upsert_read_option(argv, "--last", existing_last.min(count));
    true
}

fn constrain_read_range(argv: &mut Vec<String>, start: usize, end: usize) -> bool {
    if option_value(argv, "--last").is_some() {
        return false;
    }
    let existing_start = option_value(argv, "--line-start").unwrap_or(start);
    let existing_end = option_value(argv, "--line-end").unwrap_or(end);
    let target_start = existing_start.max(start);
    let target_end = existing_end.min(end);
    if target_end < target_start {
        return false;
    }
    upsert_read_option(argv, "--line-start", target_start);
    upsert_read_option(argv, "--line-end", target_end);
    true
}

fn option_value(argv: &[String], flag: &str) -> Option<usize> {
    argv.windows(2).find_map(|pair| match pair {
        [found_flag, value] if found_flag == flag => value.parse::<usize>().ok(),
        _ => None,
    })
}

fn upsert_option_value(argv: &mut Vec<String>, flag: &str, value: usize, insert_idx: usize) {
    let value = value.to_string();
    if let Some(idx) = argv.iter().position(|arg| arg == flag) {
        if let Some(slot) = argv.get_mut(idx + 1) {
            *slot = value;
            return;
        }
    }
    argv.insert(insert_idx.min(argv.len()), flag.to_string());
    argv.insert((insert_idx + 1).min(argv.len()), value);
}

fn upsert_read_option(argv: &mut Vec<String>, flag: &str, value: usize) {
    let insert_idx = argv.len().saturating_sub(1);
    upsert_option_value(argv, flag, value, insert_idx);
}

fn upsert_tree_option(argv: &mut Vec<String>, flag: &str, value: usize) {
    let mut idx = 2usize;
    while idx < argv.len() {
        match argv[idx].as_str() {
            "--hidden" => idx += 1,
            "--max-depth" | "--max-entries" => idx += 2,
            _ => break,
        }
    }
    upsert_option_value(argv, flag, value, idx);
}

fn upsert_grep_option(argv: &mut Vec<String>, flag: &str, value: usize) {
    let mut idx = 2usize;
    while idx < argv.len() {
        match argv[idx].as_str() {
            "--fixed-string" | "--ignore-case" | "--whole-word" => idx += 1,
            "--context-lines" | "--max-matches-per-file" | "--max-total-matches" => idx += 2,
            _ => break,
        }
    }
    upsert_option_value(argv, flag, value, idx);
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
            value
                if value.starts_with('-')
                    && value.len() > 1
                    && value[1..].chars().all(|ch| ch.is_ascii_digit()) =>
            {
                count = value[1..].parse().ok()?;
            }
            value if value.starts_with('-') => {}
            value => path = Some(value.to_string()),
        }
    }
    Some((count, path?))
}

fn contains_unsupported_glob_tokens(argv: &[String], native_tool: Option<&NativeToolPlan>) -> bool {
    if !argv.iter().any(|arg| contains_glob_chars(arg)) {
        return false;
    }
    match native_tool.map(|plan| &plan.kind) {
        Some(NativeToolKind::Tree) | Some(NativeToolKind::Grep) => false,
        _ => true,
    }
}

fn contains_glob_chars(value: &str) -> bool {
    value.contains('*') || value.contains('?') || value.contains('[')
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

fn build_native_tool_rewrite(
    root: &Path,
    task_id: &str,
    cwd: &str,
    plan: &NativeToolPlan,
) -> String {
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

fn strip_supported_postprocess(command: &str) -> (String, Option<Postprocess>) {
    let Some(pipe_idx) = find_last_top_level_pipe(command) else {
        return (command.trim().to_string(), None);
    };
    let lhs = command[..pipe_idx].trim();
    let rhs = command[pipe_idx + 1..].trim();
    match parse_supported_postprocess(rhs) {
        Some(postprocess) => (lhs.to_string(), Some(postprocess)),
        None => (command.trim().to_string(), None),
    }
}

fn find_last_top_level_pipe(command: &str) -> Option<usize> {
    let bytes = command.as_bytes();
    let mut in_single = false;
    let mut in_double = false;
    let mut last_pipe = None;
    let mut idx = 0usize;
    while idx < bytes.len() {
        let ch = bytes[idx] as char;
        if ch == '\'' && !in_double {
            in_single = !in_single;
        } else if ch == '"' && !in_single {
            in_double = !in_double;
        } else if ch == '|' && !in_single && !in_double {
            if bytes.get(idx + 1).map(|next| *next != b'|').unwrap_or(true)
                && idx > 0
                && bytes[idx - 1] != b'|'
            {
                last_pipe = Some(idx);
            }
        }
        idx += 1;
    }
    last_pipe
}

fn parse_supported_postprocess(command: &str) -> Option<Postprocess> {
    let argv = shell_words::split(command).ok()?;
    match argv.first()?.as_str() {
        "head" => parse_head_postprocess(&argv),
        "tail" => parse_tail_postprocess(&argv),
        "sed" => parse_sed_postprocess(&argv),
        _ => None,
    }
}

fn parse_head_postprocess(argv: &[String]) -> Option<Postprocess> {
    Some(Postprocess::Head(parse_count_flag(
        argv.iter().skip(1).cloned().collect(),
    )?))
}

fn parse_tail_postprocess(argv: &[String]) -> Option<Postprocess> {
    Some(Postprocess::Tail(parse_count_flag(
        argv.iter().skip(1).cloned().collect(),
    )?))
}

fn parse_sed_postprocess(argv: &[String]) -> Option<Postprocess> {
    if argv.len() != 3 || argv.get(1).map(String::as_str) != Some("-n") {
        return None;
    }
    let (start, end) = parse_sed_range(&argv[2])?;
    Some(Postprocess::SedRange(start, end))
}

fn parse_count_flag(argv: Vec<String>) -> Option<usize> {
    let mut count = 10usize;
    let mut iter = argv.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-n" => count = iter.next()?.parse().ok()?,
            value
                if value.starts_with('-')
                    && value.len() > 1
                    && value[1..].chars().all(|ch| ch.is_ascii_digit()) =>
            {
                count = value[1..].parse().ok()?;
            }
            value if value.starts_with('-') => {}
            _ => return None,
        }
    }
    Some(count)
}

fn strip_supported_trailing_redirects(command: &str) -> String {
    let mut trimmed = command.trim().to_string();
    loop {
        let Some(stripped) = strip_one_trailing_redirect(&trimmed) else {
            break;
        };
        trimmed = stripped.trim_end().to_string();
    }
    trimmed
}

fn strip_one_trailing_redirect(command: &str) -> Option<&str> {
    const REDIRECT_SUFFIXES: &[&str] = &[
        "2>&1",
        "1>&2",
        ">/dev/null",
        "1>/dev/null",
        "2>/dev/null",
        "1> /dev/null",
        "2> /dev/null",
        "> /dev/null",
    ];
    REDIRECT_SUFFIXES
        .iter()
        .find_map(|suffix| command.strip_suffix(suffix))
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
            '{' if !in_single && !in_double => return true,
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
    use std::fs;

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

    #[test]
    fn routes_tree_command_to_native_tool() {
        let decision = decide_command_route("tree -L 2 crates");
        assert_eq!(decision.kind, RouteKind::NativeTool);
    }

    #[test]
    fn tolerates_trailing_redirects_for_reducer_routes() {
        let decision = decide_command_route("FOO=1 cargo test -q 2>&1");
        assert_eq!(decision.kind, RouteKind::ReducerRewrite);
        assert_eq!(decision.env_assignments.len(), 1);
    }

    #[test]
    fn routes_reducer_commands_with_truncation_pipe() {
        let decision = decide_command_route("git show HEAD | head -n 20");
        assert_eq!(decision.kind, RouteKind::ReducerRewrite);
        assert_eq!(
            decision
                .reducer_spec
                .as_ref()
                .map(|spec| spec.canonical_kind.as_str()),
            Some("git_show")
        );
    }

    #[test]
    fn routes_grep_with_head_pipe_to_native_tool_limit() {
        let decision = decide_command_route("rg task_id crates/suite-cli/src | head -n 5");
        assert_eq!(decision.kind, RouteKind::NativeTool);
        assert_eq!(
            decision.native_tool.as_ref().map(|plan| plan.argv.clone()),
            Some(vec![
                "compact".to_string(),
                "grep".to_string(),
                "--max-total-matches".to_string(),
                "5".to_string(),
                "task_id".to_string(),
                "crates/suite-cli/src".to_string(),
            ])
        );
    }

    #[test]
    fn routes_tree_with_head_pipe_to_native_tool_limit() {
        let decision = decide_command_route("tree -L 2 crates | head -n 10");
        assert_eq!(decision.kind, RouteKind::NativeTool);
        assert_eq!(
            decision.native_tool.as_ref().map(|plan| plan.argv.clone()),
            Some(vec![
                "compact".to_string(),
                "tree".to_string(),
                "--max-depth".to_string(),
                "2".to_string(),
                "--max-entries".to_string(),
                "10".to_string(),
                "crates".to_string(),
            ])
        );
    }

    #[test]
    fn routes_cat_with_sed_pipe_to_read_range() {
        let decision = decide_command_route("cat Cargo.toml | sed -n '1,20p'");
        assert_eq!(decision.kind, RouteKind::NativeTool);
        assert_eq!(
            decision.native_tool.as_ref().map(|plan| plan.argv.clone()),
            Some(vec![
                "compact".to_string(),
                "read".to_string(),
                "--line-start".to_string(),
                "1".to_string(),
                "--line-end".to_string(),
                "20".to_string(),
                "Cargo.toml".to_string(),
            ])
        );
    }

    #[test]
    fn routes_tree_glob_to_native_tool() {
        let decision = decide_command_route("tree crates/*");
        assert_eq!(decision.kind, RouteKind::NativeTool);
    }

    #[test]
    fn routes_grep_glob_path_to_native_tool() {
        let decision = decide_command_route("rg task_id crates/**/*.rs");
        assert_eq!(decision.kind, RouteKind::NativeTool);
    }

    #[test]
    fn declines_read_glob_without_shell_expansion() {
        let decision = decide_command_route("cat crates/*/Cargo.toml");
        assert_eq!(decision.kind, RouteKind::RawPassthrough);
        assert_eq!(decision.reason.as_deref(), Some("shell_glob"));
    }

    #[test]
    fn routes_git_diff_glob_with_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path();
        fs::create_dir_all(cwd.join("src")).unwrap();
        fs::write(cwd.join("src/a.rs"), "fn a() {}").unwrap();
        fs::write(cwd.join("src/b.rs"), "fn b() {}").unwrap();
        let decision = decide_command_route_with_cwd("git diff src/*.rs", cwd);
        assert_eq!(decision.kind, RouteKind::ReducerRewrite);
        assert!(decision.argv.contains(&"src/a.rs".to_string()));
        assert!(decision.argv.contains(&"src/b.rs".to_string()));
        assert_eq!(
            decision.original_argv,
            Some(vec!["git".to_string(), "diff".to_string(), "src/*.rs".to_string()])
        );
    }

    #[test]
    fn glob_expansion_preserves_flags() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path();
        fs::create_dir_all(cwd.join("src")).unwrap();
        fs::write(cwd.join("src/x.rs"), "fn x() {}").unwrap();
        let decision = decide_command_route_with_cwd("git diff --cached src/*.rs", cwd);
        assert_eq!(decision.kind, RouteKind::ReducerRewrite);
        assert!(decision.argv.contains(&"--cached".to_string()));
        assert!(decision.argv.contains(&"src/x.rs".to_string()));
    }

    #[test]
    fn glob_no_matches_keeps_original() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path();
        let decision = decide_command_route_with_cwd("git diff nonexistent/*.rs", cwd);
        assert_eq!(decision.kind, RouteKind::RawPassthrough);
        assert_eq!(decision.reason.as_deref(), Some("shell_glob"));
    }

    #[test]
    fn glob_without_cwd_still_rejects() {
        let decision = decide_command_route("git diff src/*.rs");
        assert_eq!(decision.kind, RouteKind::RawPassthrough);
        assert_eq!(decision.reason.as_deref(), Some("shell_glob"));
    }

    #[test]
    fn cargo_test_glob_expansion() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path();
        fs::create_dir_all(cwd.join("tests")).unwrap();
        fs::write(cwd.join("tests/test_foo.rs"), "#[test] fn t() {}").unwrap();
        fs::write(cwd.join("Cargo.toml"), "[package]\nname=\"x\"\nversion=\"0.1.0\"\n").unwrap();
        let decision = decide_command_route_with_cwd("cargo test tests/test_*.rs", cwd);
        assert_eq!(decision.kind, RouteKind::ReducerRewrite);
        assert!(decision.argv.contains(&"tests/test_foo.rs".to_string()));
    }
}
