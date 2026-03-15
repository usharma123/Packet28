use crate::types::{CommandReducerSpec, CommandReduction};

pub fn classify_infra_command(command: &str, argv: &[String]) -> Option<CommandReducerSpec> {
    let (canonical_kind, operation_kind) = match argv.first()?.as_str() {
        "docker" => match argv.get(1)?.as_str() {
            "ps" if !contains_any(argv, &["--format", "--quiet", "-q"]) => {
                ("docker_ps", suite_packet_core::ToolOperationKind::Fetch)
            }
            "images" if !contains_any(argv, &["--format", "--quiet", "-q"]) => {
                ("docker_images", suite_packet_core::ToolOperationKind::Fetch)
            }
            "logs" if !contains_any(argv, &["--follow", "-f", "--timestamps"]) => {
                ("docker_logs", suite_packet_core::ToolOperationKind::Read)
            }
            _ => return None,
        },
        "kubectl" => match argv.get(1)?.as_str() {
            "get" if !contains_any(argv, &["-o", "--output", "--watch", "-w"]) => {
                ("kubectl_get", suite_packet_core::ToolOperationKind::Fetch)
            }
            "logs" if !contains_any(argv, &["-f", "--follow", "--prefix"]) => {
                ("kubectl_logs", suite_packet_core::ToolOperationKind::Read)
            }
            _ => return None,
        },
        "curl" if classify_curl(argv) => {
            ("curl_fetch", suite_packet_core::ToolOperationKind::Fetch)
        }
        _ => return None,
    };

    let paths = argv
        .iter()
        .skip(1)
        .filter(|arg| !arg.starts_with('-') && looks_like_target(arg))
        .cloned()
        .collect::<Vec<_>>();

    Some(CommandReducerSpec {
        family: "infra".to_string(),
        canonical_kind: canonical_kind.to_string(),
        packet_type: "packet28.hook.infra.v2".to_string(),
        operation_kind,
        command: command.to_string(),
        argv: argv.to_vec(),
        cache_fingerprint: fingerprint("infra", canonical_kind, argv),
        cacheable: true,
        mutation: false,
        paths,
        equivalence_key: None,
    })
}

pub fn reduce_infra_command(
    spec: &CommandReducerSpec,
    stdout: &str,
    stderr: &str,
    exit_code: i32,
) -> CommandReduction {
    let failed = exit_code != 0;
    let combined = format!("{stdout}\n{stderr}");
    let lines = nonempty_lines(stdout);
    let summary = match spec.canonical_kind.as_str() {
        "docker_ps" => format!("docker ps listed {} container(s)", data_rows(&lines)),
        "docker_images" => format!("docker images listed {} image(s)", data_rows(&lines)),
        "docker_logs" => format!("docker logs returned {} line(s)", lines.len()),
        "kubectl_get" => summarize_kubectl_get(spec, &lines),
        "kubectl_logs" => format!("kubectl logs returned {} line(s)", lines.len()),
        "curl_fetch" => {
            if failed {
                first_nonempty_line(&combined).unwrap_or_else(|| "curl failed".to_string())
            } else {
                summarize_curl(stdout)
            }
        }
        _ => {
            first_nonempty_line(&combined).unwrap_or_else(|| "infra command completed".to_string())
        }
    };

    CommandReduction {
        family: spec.family.clone(),
        canonical_kind: spec.canonical_kind.clone(),
        packet_type: spec.packet_type.clone(),
        operation_kind: spec.operation_kind,
        summary: if failed && spec.canonical_kind != "curl_fetch" {
            first_nonempty_line(&combined).unwrap_or(summary)
        } else {
            summary
        },
        paths: spec.paths.clone(),
        regions: Vec::new(),
        symbols: Vec::new(),
        failed,
        error_class: failed.then(|| "infra_error".to_string()),
        error_message: failed.then(|| compact(&combined, 220)),
        retryable: failed.then_some(false),
        exit_code,
        cache_fingerprint: spec.cache_fingerprint.clone(),
        cacheable: spec.cacheable,
        mutation: spec.mutation,
        equivalence_key: spec.equivalence_key.clone(),
    }
}

fn classify_curl(argv: &[String]) -> bool {
    if contains_any(
        argv,
        &[
            "-o",
            "--output",
            "-O",
            "--remote-name",
            "-w",
            "--write-out",
            "-d",
            "--data",
            "--data-binary",
            "--data-raw",
            "--data-urlencode",
            "--head",
            "-I",
        ],
    ) {
        return false;
    }
    if let Some(method) = explicit_curl_method(argv) {
        if method != "GET" && method != "HEAD" {
            return false;
        }
    }
    argv.iter()
        .any(|arg| arg.starts_with("http://") || arg.starts_with("https://"))
}

fn contains_any(argv: &[String], denied: &[&str]) -> bool {
    argv.iter().any(|arg| {
        denied
            .iter()
            .any(|denied| arg == denied || arg.starts_with(&format!("{denied}=")))
    })
}

fn looks_like_target(value: &str) -> bool {
    value.contains('/') || value.contains(':') || value.starts_with("http")
}

fn nonempty_lines(value: &str) -> Vec<String> {
    value
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn data_rows(lines: &[String]) -> usize {
    lines.len().saturating_sub(1)
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

fn explicit_curl_method(argv: &[String]) -> Option<String> {
    let mut iter = argv.iter();
    while let Some(arg) = iter.next() {
        if arg == "-X" {
            return iter.next().map(|value| value.to_ascii_uppercase());
        }
        if let Some(method) = arg.strip_prefix("-X") {
            if !method.is_empty() {
                return Some(method.to_ascii_uppercase());
            }
        }
        if let Some(method) = arg.strip_prefix("--request=") {
            return Some(method.to_ascii_uppercase());
        }
    }
    None
}

fn summarize_kubectl_get(spec: &CommandReducerSpec, lines: &[String]) -> String {
    let resource = spec.argv.get(2).map(String::as_str).unwrap_or("resource");
    let rows = data_rows(lines);
    if lines.is_empty() || rows == 0 {
        return format!("kubectl get {resource} returned 0 row(s)");
    }
    let pending = lines
        .iter()
        .skip(1)
        .filter(|line| line.contains(" Pending ") || line.ends_with(" Pending"))
        .count();
    if pending > 0 {
        format!("kubectl get {resource} returned {rows} row(s), {pending} pending")
    } else {
        format!("kubectl get {resource} returned {rows} row(s)")
    }
}

fn summarize_curl(stdout: &str) -> String {
    let bytes = stdout.len();
    if let Some(title) = extract_html_title(stdout) {
        format!("curl HTML '{title}' ({bytes}b)")
    } else {
        format!("curl returned {bytes} byte(s)")
    }
}

fn extract_html_title(stdout: &str) -> Option<String> {
    let lower = stdout.to_ascii_lowercase();
    let start = lower.find("<title>")?;
    let end = lower.find("</title>")?;
    if end <= start + 7 {
        return None;
    }
    Some(stdout[start + 7..end].trim().to_string())
}

fn fingerprint(family: &str, kind: &str, argv: &[String]) -> String {
    format!("{family}:{kind}:{}", argv.join("\u{1f}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_infra_declines_following_logs() {
        let argv = vec!["docker", "logs", "-f", "api"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert!(classify_infra_command("docker logs -f api", &argv).is_none());
    }

    #[test]
    fn reduce_kubectl_get_summarizes_rows() {
        let argv = vec!["kubectl", "get", "pods"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let spec = classify_infra_command("kubectl get pods", &argv).unwrap();
        let output = "NAME READY STATUS RESTARTS AGE\napi-123 1/1 Running 0 2d\nworker-456 1/1 Running 1 2d\n";
        let reduction = reduce_infra_command(&spec, output, "", 0);
        assert_eq!(reduction.summary, "kubectl get pods returned 2 row(s)");
    }

    #[test]
    fn reduce_kubectl_get_mentions_pending_rows() {
        let argv = vec!["kubectl", "get", "pods"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let spec = classify_infra_command("kubectl get pods", &argv).unwrap();
        let output =
            "NAME READY STATUS RESTARTS AGE\napi 1/1 Running 0 2d\ncron 0/1 Pending 0 4m\n";
        let reduction = reduce_infra_command(&spec, output, "", 0);
        assert_eq!(
            reduction.summary,
            "kubectl get pods returned 2 row(s), 1 pending"
        );
    }

    #[test]
    fn reduce_curl_extracts_html_title() {
        let argv = vec!["curl", "https://example.com"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let spec = classify_infra_command("curl https://example.com", &argv).unwrap();
        let output = "<html><head><title>Packet28 Fixture</title></head><body></body></html>";
        let reduction = reduce_infra_command(&spec, output, "", 0);
        assert_eq!(reduction.summary, "curl HTML 'Packet28 Fixture' (70b)");
    }
}
