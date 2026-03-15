use crate::types::{CommandReducerSpec, CommandReduction};

pub fn classify_fs_command(command: &str, argv: &[String]) -> Option<CommandReducerSpec> {
    let program = argv.first()?.as_str();
    let (canonical_kind, operation_kind) = match program {
        "ls" if classify_ls(argv) => ("fs_ls", suite_packet_core::ToolOperationKind::Read),
        "find" if classify_find(argv) => ("fs_find", suite_packet_core::ToolOperationKind::Search),
        "cat" if classify_cat(argv) => ("fs_cat", suite_packet_core::ToolOperationKind::Read),
        "head" if classify_head_tail(argv, false) => {
            ("fs_head", suite_packet_core::ToolOperationKind::Read)
        }
        "tail" if classify_head_tail(argv, true) => {
            ("fs_tail", suite_packet_core::ToolOperationKind::Read)
        }
        "sed" if classify_sed(argv) => ("fs_sed", suite_packet_core::ToolOperationKind::Read),
        "diff" if classify_diff(argv) => ("fs_diff", suite_packet_core::ToolOperationKind::Diff),
        _ => return None,
    };
    let paths = argv
        .iter()
        .skip(1)
        .filter(|arg| !arg.starts_with('-') && looks_like_path(arg))
        .cloned()
        .collect::<Vec<_>>();
    let equivalence_key = match canonical_kind {
        "fs_cat" => paths.first().map(|path| format!("read:{path}")),
        "fs_head" => paths
            .first()
            .map(|path| format!("read:{}:{}", path, parse_head_count(argv).unwrap_or(10))),
        "fs_sed" => paths.first().and_then(|path| {
            parse_sed_region(argv).map(|(start, end)| format!("read:{path}:{start}-{end}"))
        }),
        "fs_find" => Some(format!(
            "glob:{}",
            argv.iter().skip(1).cloned().collect::<Vec<_>>().join(" ")
        )),
        _ => None,
    };
    Some(CommandReducerSpec {
        family: "fs".to_string(),
        canonical_kind: canonical_kind.to_string(),
        packet_type: "packet28.hook.fs.v2".to_string(),
        operation_kind,
        command: command.to_string(),
        argv: argv.to_vec(),
        cache_fingerprint: fingerprint("fs", canonical_kind, argv),
        cacheable: true,
        mutation: false,
        paths,
        equivalence_key,
    })
}

pub fn reduce_fs_command(
    spec: &CommandReducerSpec,
    stdout: &str,
    stderr: &str,
    exit_code: i32,
) -> CommandReduction {
    let failed = exit_code != 0;
    let lines = nonempty_lines(stdout).len();
    let summary = match spec.canonical_kind.as_str() {
        "fs_ls" => {
            let target = spec
                .paths
                .first()
                .cloned()
                .unwrap_or_else(|| ".".to_string());
            format!(
                "ls listed {lines} entr{suffix} in {target}",
                suffix = if lines == 1 { "y" } else { "ies" }
            )
        }
        "fs_find" => {
            let target = spec
                .paths
                .first()
                .cloned()
                .unwrap_or_else(|| ".".to_string());
            format!("find matched {lines} path(s) under {target}")
        }
        "fs_cat" | "fs_head" | "fs_tail" | "fs_sed" => {
            let target = spec
                .paths
                .first()
                .cloned()
                .unwrap_or_else(|| "file".to_string());
            match spec.canonical_kind.as_str() {
                "fs_head" => format!(
                    "head returned {lines} line(s) from {target}{}",
                    parse_head_count(&spec.argv)
                        .map(|count| format!(" (requested {count})"))
                        .unwrap_or_default()
                ),
                "fs_tail" => format!(
                    "tail returned {lines} line(s) from {target}{}",
                    parse_head_count(&spec.argv)
                        .map(|count| format!(" (requested last {count})"))
                        .unwrap_or_default()
                ),
                "fs_sed" => {
                    if let Some((start, end)) = parse_sed_region(&spec.argv) {
                        format!("sed printed lines {start}-{end} from {target}")
                    } else {
                        format!("sed returned {lines} line(s) from {target}")
                    }
                }
                _ => format!("cat returned {lines} line(s) from {target}"),
            }
        }
        "fs_diff" => {
            let compared = spec.paths.iter().take(2).cloned().collect::<Vec<_>>();
            if lines > 0 {
                if compared.len() == 2 {
                    format!(
                        "diff compared {} and {} ({lines} output line(s))",
                        compared[0], compared[1]
                    )
                } else {
                    format!("diff produced {lines} output line(s)")
                }
            } else {
                "diff completed".to_string()
            }
        }
        _ => {
            let command = spec
                .argv
                .first()
                .cloned()
                .unwrap_or_else(|| "<unknown command>".to_string());
            first_nonempty_line(stdout).unwrap_or_else(|| format!("{command} completed"))
        }
    };
    let mut regions = Vec::new();
    if let Some(path) = spec.paths.first() {
        match spec.canonical_kind.as_str() {
            "fs_cat" => {
                let end = lines.max(1);
                regions.push(format!("{path}:1-{end}"));
            }
            "fs_head" => {
                let end = lines.max(1);
                regions.push(format!("{path}:1-{end}"));
            }
            "fs_sed" => {
                if let Some((start, end)) = parse_sed_region(&spec.argv) {
                    regions.push(format!("{path}:{start}-{end}"));
                }
            }
            _ => {}
        }
    }
    CommandReduction {
        family: spec.family.clone(),
        canonical_kind: spec.canonical_kind.clone(),
        packet_type: spec.packet_type.clone(),
        operation_kind: spec.operation_kind,
        summary: if failed {
            first_nonempty_line(stderr).unwrap_or(summary)
        } else {
            summary
        },
        paths: spec.paths.clone(),
        regions,
        symbols: Vec::new(),
        failed,
        error_class: failed.then(|| "fs_error".to_string()),
        error_message: failed.then(|| compact(stderr, 200)),
        retryable: failed.then_some(false),
        exit_code,
        cache_fingerprint: spec.cache_fingerprint.clone(),
        cacheable: spec.cacheable,
        mutation: spec.mutation,
        equivalence_key: spec.equivalence_key.clone(),
    }
}

fn classify_ls(argv: &[String]) -> bool {
    !contains_any(argv, &["-R", "--recursive", "--dired"])
}

fn classify_find(argv: &[String]) -> bool {
    !contains_any(
        argv,
        &[
            "-exec", "-execdir", "-ok", "-okdir", "-delete", "-printf", "-fprintf", "-ls",
        ],
    )
}

fn classify_cat(argv: &[String]) -> bool {
    !contains_any(argv, &["-v", "-A", "--show-all", "--show-nonprinting"])
}

fn classify_head_tail(argv: &[String], tail: bool) -> bool {
    if contains_any(argv, &["-c", "--bytes"]) {
        return false;
    }
    if tail && contains_any(argv, &["-f", "--follow", "--retry", "-F"]) {
        return false;
    }
    parse_head_count(argv).is_some() || argv.len() >= 2
}

fn classify_sed(argv: &[String]) -> bool {
    if contains_any(argv, &["-i", "--in-place", "-e", "-f"]) {
        return false;
    }
    parse_sed_region(argv).is_some()
}

fn classify_diff(argv: &[String]) -> bool {
    if contains_any(
        argv,
        &[
            "--git",
            "--patch",
            "-p",
            "--raw",
            "--word-diff",
            "--side-by-side",
        ],
    ) {
        return false;
    }
    argv.iter()
        .skip(1)
        .filter(|arg| !arg.starts_with('-'))
        .count()
        >= 2
}

fn contains_any(argv: &[String], denied: &[&str]) -> bool {
    argv.iter().any(|arg| {
        denied
            .iter()
            .any(|denied| arg == denied || arg.starts_with(&format!("{denied}=")))
    })
}

fn looks_like_path(value: &str) -> bool {
    !(value == "." || value == "..")
        && (value.contains('/')
            || value.starts_with('.')
            || value
                .chars()
                .any(|ch| ch.is_ascii_alphabetic() && value.contains('.')))
}

fn first_nonempty_line(value: &str) -> Option<String> {
    value
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToOwned::to_owned)
}

fn nonempty_lines(value: &str) -> Vec<&str> {
    value
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect()
}

fn parse_head_count(argv: &[String]) -> Option<usize> {
    let mut idx = 1;
    while idx < argv.len() {
        let arg = argv[idx].as_str();
        if let Some(value) = arg.strip_prefix("--lines=") {
            return value.parse::<usize>().ok();
        }
        if arg == "-n" || arg == "--lines" {
            return argv.get(idx + 1)?.parse::<usize>().ok();
        }
        if let Some(value) = arg.strip_prefix('-') {
            if !value.is_empty() && value.chars().all(|ch| ch.is_ascii_digit()) {
                return value.parse::<usize>().ok();
            }
        }
        idx += 1;
    }
    Some(10)
}

fn parse_sed_region(argv: &[String]) -> Option<(usize, usize)> {
    let expression = argv
        .iter()
        .skip(1)
        .find(|arg| !arg.starts_with('-'))
        .map(String::as_str)?;
    if !expression.ends_with('p') {
        return None;
    }
    let range = expression.strip_suffix('p')?;
    if let Some((start, end)) = range.split_once(',') {
        return Some((start.parse::<usize>().ok()?, end.parse::<usize>().ok()?));
    }
    let line = range.parse::<usize>().ok()?;
    Some((line, line))
}

fn compact(value: &str, limit: usize) -> String {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let char_count = compact.chars().count();
    if char_count <= limit {
        compact
    } else if limit <= 3 {
        "...".to_string()
    } else {
        let shortened = compact
            .chars()
            .take(limit.saturating_sub(3))
            .collect::<String>();
        format!("{shortened}...")
    }
}

fn fingerprint(family: &str, kind: &str, argv: &[String]) -> String {
    format!("{family}:{kind}:{}", argv.join("\u{1f}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_sed_declines_mutating_shapes() {
        let argv = vec!["sed", "-i", "1,4p", "src/lib.rs"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert!(classify_fs_command("sed -i 1,4p src/lib.rs", &argv).is_none());
    }

    #[test]
    fn reduce_head_adds_region() {
        let argv = vec!["head", "-n", "3", "README.md"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let spec = classify_fs_command("head -n 3 README.md", &argv).unwrap();
        let reduction = reduce_fs_command(&spec, "a\nb\nc\n", "", 0);
        assert_eq!(
            reduction.summary,
            "head returned 3 line(s) from README.md (requested 3)"
        );
        assert_eq!(reduction.regions, vec!["README.md:1-3".to_string()]);
    }
}
