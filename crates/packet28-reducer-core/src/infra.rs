use crate::types::{CommandReducerSpec, CommandReduction};

pub fn classify_infra_command(command: &str, argv: &[String]) -> Option<CommandReducerSpec> {
    let (canonical_kind, operation_kind) = match argv.first()?.as_str() {
        "docker" => match argv.get(1)?.as_str() {
            "compose" => match argv.get(2)?.as_str() {
                "ps" if !contains_any(argv, &["--format", "--quiet", "-q"]) => (
                    "docker_compose_ps",
                    suite_packet_core::ToolOperationKind::Fetch,
                ),
                "logs" if !contains_any(argv, &["--follow", "-f", "--timestamps"]) => (
                    "docker_compose_logs",
                    suite_packet_core::ToolOperationKind::Read,
                ),
                _ => return None,
            },
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
            "describe" => (
                "kubectl_describe",
                suite_packet_core::ToolOperationKind::Read,
            ),
            _ => return None,
        },
        "curl" if classify_curl(argv) => {
            ("curl_fetch", suite_packet_core::ToolOperationKind::Fetch)
        }
        "aws" if classify_aws(argv) => ("aws_cli", suite_packet_core::ToolOperationKind::Fetch),
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
        "docker_compose_ps" => {
            format!("docker compose ps listed {} service(s)", data_rows(&lines))
        }
        "docker_compose_logs" => format!("docker compose logs returned {} line(s)", lines.len()),
        "kubectl_get" => summarize_kubectl_get(spec, &lines),
        "kubectl_logs" => format!("kubectl logs returned {} line(s)", lines.len()),
        "kubectl_describe" => summarize_kubectl_describe(stdout),
        "curl_fetch" => {
            if failed {
                first_nonempty_line(&combined).unwrap_or_else(|| "curl failed".to_string())
            } else {
                summarize_curl(stdout)
            }
        }
        "aws_cli" => {
            if failed {
                first_nonempty_line(&combined).unwrap_or_else(|| "aws command failed".to_string())
            } else {
                summarize_aws(stdout)
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
        compact_preview: match spec.canonical_kind.as_str() {
            "docker_logs" | "docker_compose_logs" | "kubectl_logs" => {
                compact_log_output(stdout, 50)
            }
            "curl_fetch" if !failed => compact_curl_response(stdout),
            _ => String::new(),
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
    let pending_rows = lines
        .iter()
        .skip(1)
        .filter_map(|line| {
            (line.contains(" Pending ") || line.ends_with(" Pending")).then(|| {
                line.split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_string()
            })
        })
        .collect::<Vec<_>>();
    if let Some(first_pending) = pending_rows.first() {
        format!(
            "kubectl get {resource}: {rows} row(s), {} pending; first {first_pending}",
            pending_rows.len()
        )
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

fn summarize_kubectl_describe(stdout: &str) -> String {
    let name = stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("Name:"))
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let namespace = stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("Namespace:"))
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let events = stdout
        .lines()
        .position(|line| line.trim() == "Events:")
        .map(|idx| {
            stdout
                .lines()
                .skip(idx + 1)
                .filter(|line| !line.trim().is_empty())
                .count()
        })
        .unwrap_or(0);
    match (name, namespace) {
        (Some(name), Some(namespace)) => {
            format!("kubectl describe: {name} in {namespace} ({events} event line(s))")
        }
        (Some(name), None) => format!("kubectl describe: {name} ({events} event line(s))"),
        _ => "kubectl describe completed".to_string(),
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

fn classify_aws(argv: &[String]) -> bool {
    // Support `aws s3 ls`, `aws ec2 describe-instances`, `aws sts get-caller-identity`, etc.
    argv.len() >= 3 && !contains_any(argv, &["--output", "-o"])
}

fn summarize_aws(stdout: &str) -> String {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return "aws returned empty response".to_string();
    }
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        return match value {
            serde_json::Value::Object(map) => {
                let keys: Vec<_> = map.keys().take(5).cloned().collect();
                format!("aws returned object with keys: {}", keys.join(", "))
            }
            serde_json::Value::Array(items) => {
                format!("aws returned {} item(s)", items.len())
            }
            _ => format!(
                "aws returned {}",
                trimmed.chars().take(80).collect::<String>()
            ),
        };
    }
    let lines = nonempty_lines(stdout);
    format!("aws returned {} line(s)", lines.len())
}

fn compact_log_output(output: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let collapsed = collapse_repeated_log_lines(&lines);
    collapsed
        .into_iter()
        .take(max_lines)
        .collect::<Vec<_>>()
        .join("\n")
}

fn collapse_repeated_log_lines(lines: &[&str]) -> Vec<String> {
    let mut result = Vec::new();
    let mut prev: Option<&str> = None;
    let mut count = 0usize;
    for &line in lines {
        // Strip timestamp prefix for comparison
        let normalized = strip_log_timestamp(line);
        let prev_normalized = prev.map(strip_log_timestamp);
        if prev_normalized.as_deref() == Some(&normalized) {
            count += 1;
        } else {
            if let Some(prev_line) = prev {
                if count > 1 {
                    result.push(format!("[x{}] {}", count, prev_line));
                } else {
                    result.push(prev_line.to_string());
                }
            }
            prev = Some(line);
            count = 1;
        }
    }
    if let Some(prev_line) = prev {
        if count > 1 {
            result.push(format!("[x{}] {}", count, prev_line));
        } else {
            result.push(prev_line.to_string());
        }
    }
    result
}

fn strip_log_timestamp(line: &str) -> String {
    // Common timestamp patterns: ISO 8601, Docker timestamps, etc.
    // Strip leading YYYY-MM-DD HH:MM:SS or similar
    let chars: Vec<char> = line.chars().collect();
    if chars.len() > 19 {
        let prefix: String = chars[..19].iter().collect();
        if prefix.chars().filter(|c| c.is_ascii_digit()).count() >= 8
            && (prefix.contains('-') || prefix.contains('/'))
            && (prefix.contains(':') || prefix.contains('T'))
        {
            return line[19..].trim().to_string();
        }
    }
    line.to_string()
}

fn compact_curl_response(stdout: &str) -> String {
    // For HTML: extract title
    if let Some(title) = extract_html_title(stdout) {
        return format!("HTML: {title}");
    }
    // For JSON: show structure
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(stdout.trim()) {
        return match value {
            serde_json::Value::Object(map) => {
                let keys: Vec<_> = map.keys().take(10).cloned().collect();
                format!("JSON object with keys: {}", keys.join(", "))
            }
            serde_json::Value::Array(items) => {
                format!("JSON array with {} item(s)", items.len())
            }
            _ => format!(
                "JSON scalar: {}",
                stdout.trim().chars().take(50).collect::<String>()
            ),
        };
    }
    // For plain text: first few lines
    let lines: Vec<&str> = stdout.lines().take(5).collect();
    lines.join("\n")
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
            "kubectl get pods: 2 row(s), 1 pending; first cron"
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

    #[test]
    fn classify_infra_supports_docker_compose_and_kubectl_describe() {
        let argv = vec!["docker", "compose", "ps"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert_eq!(
            classify_infra_command("docker compose ps", &argv)
                .unwrap()
                .canonical_kind,
            "docker_compose_ps"
        );

        let argv = vec!["kubectl", "describe", "pod", "api"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert_eq!(
            classify_infra_command("kubectl describe pod api", &argv)
                .unwrap()
                .canonical_kind,
            "kubectl_describe"
        );
    }

    #[test]
    fn reduce_kubectl_describe_summarizes_identity_and_events() {
        let argv = vec!["kubectl", "describe", "pod", "api"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let spec = classify_infra_command("kubectl describe pod api", &argv).unwrap();
        let output = "Name:         api\nNamespace:    prod\nStatus:       Running\n\nEvents:\n  Type    Reason   Age\n  Normal  Pulled   2m\n";
        let reduction = reduce_infra_command(&spec, output, "", 0);
        assert_eq!(
            reduction.summary,
            "kubectl describe: api in prod (2 event line(s))"
        );
    }
}
