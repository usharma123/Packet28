use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use clap::{Args, ValueEnum};
use packet28_reducer_core::{classify_command_argv, reduce_command_output};
use serde_json::json;

use crate::savings_analytics::{record_run_savings, RunSavingsRecord};

#[derive(Debug, Clone, Copy, ValueEnum, Default, PartialEq, Eq)]
pub enum RuntimeBackend {
    #[default]
    Auto,
    LinuxPreload,
    LinuxOci,
    MacosSwap,
    MacosFuse,
    WindowsFuse,
    ProxyOnly,
}

impl RuntimeBackend {
    fn as_env_value(self) -> &'static str {
        match self {
            RuntimeBackend::Auto => "auto",
            RuntimeBackend::LinuxPreload => "linux_preload",
            RuntimeBackend::LinuxOci => "linux_oci",
            RuntimeBackend::MacosSwap => "macos_swap",
            RuntimeBackend::MacosFuse => "macos_fuse",
            RuntimeBackend::WindowsFuse => "windows_fuse",
            RuntimeBackend::ProxyOnly => "proxy_only",
        }
    }
}

#[derive(Args)]
#[command(trailing_var_arg = true)]
pub struct RunArgs {
    #[arg(long, default_value = ".")]
    pub root: String,
    #[arg(long, value_enum, default_value_t)]
    pub backend: RuntimeBackend,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub pretty: bool,
    #[arg(allow_hyphen_values = true, required = true)]
    pub command: Vec<String>,
}

pub fn run(args: RunArgs) -> Result<i32> {
    let cwd = crate::cmd_common::caller_cwd()?;
    let root = PathBuf::from(crate::cmd_common::resolve_path_from_cwd(&args.root, &cwd));
    let root = packet28_daemon_protocol::paths::resolve_workspace_root(&root);
    let backend = args.backend;

    match backend {
        RuntimeBackend::Auto => run_reducer_aware(&root, &cwd, &args),
        RuntimeBackend::LinuxPreload => run_linux_preload(&root, &args.command),
        RuntimeBackend::LinuxOci => run_linux_oci(&root, &args.command),
        RuntimeBackend::MacosSwap => run_macos_swap(&root, &args.command),
        RuntimeBackend::MacosFuse => run_macos_fuse(&root, &args.command),
        RuntimeBackend::WindowsFuse => run_windows_fuse(&root, &args.command),
        RuntimeBackend::ProxyOnly => run_proxy_only(&root, &args.command),
    }
}

fn run_reducer_aware(root: &std::path::Path, cwd: &std::path::Path, args: &RunArgs) -> Result<i32> {
    let command_text = command_text(&args.command);
    let Some(spec) = classify_command_argv(&command_text, &args.command) else {
        return run_auto_fallback(root, cwd, args, "unsupported");
    };
    if !matches!(
        spec.family.as_str(),
        "git"
            | "rust"
            | "javascript"
            | "python"
            | "fs"
            | "infra"
            | "github"
            | "go"
            | "ruby"
            | "dotnet"
            | "jvm"
    ) {
        return run_auto_fallback(root, cwd, args, "unsupported_family");
    }

    let before_changed_paths = current_changed_paths(root);
    let output = build_command(&args.command)?
        .current_dir(cwd)
        .output()
        .with_context(|| format!("failed to run `{command_text}`"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let exit_code = output.status.code().unwrap_or(1);
    let reduction = reduce_command_output(&spec, &stdout, &stderr, exit_code)?;
    let raw_est_tokens = estimate_tokens(&(stdout.clone() + &stderr));
    let rendered_command_body = render_command_body(&reduction.summary, &reduction.compact_preview);
    let reduced_est_tokens = estimate_tokens(&rendered_command_body);
    let raw_artifact_handle =
        write_run_raw_artifact(root, &command_text, exit_code, &stdout, &stderr)?;
    let failure_fingerprint = failure_fingerprint(exit_code, &stdout, &stderr);
    let changed_paths = new_changed_paths(root, &before_changed_paths);
    let saved = raw_est_tokens.saturating_sub(reduced_est_tokens);
    let savings_pct = if raw_est_tokens == 0 {
        0.0
    } else {
        (saved as f64 / raw_est_tokens as f64) * 100.0
    };
    let payload = json!({
        "command": {
            "original": command_text,
            "cwd": cwd.display().to_string(),
            "exit_code": exit_code,
            "timestamp_unix_ms": timestamp_unix_ms(),
        },
        "reduction": reduction,
        "rendered_command_body": rendered_command_body,
        "estimate_scope": "rendered_command_body",
        "raw_est_tokens": raw_est_tokens,
        "reduced_est_tokens": reduced_est_tokens,
        "savings_percent": savings_pct,
        "fallback_reason": null,
        "failure_fingerprint": failure_fingerprint,
        "raw_artifact": {
            "available": true,
            "handle": raw_artifact_handle,
        },
        "provenance": {
            "original_command": command_text,
            "cwd": cwd.display().to_string(),
            "exit_code": exit_code,
            "timestamp_unix_ms": timestamp_unix_ms(),
        }
    });
    record_run_savings(
        root,
        &RunSavingsRecord {
            command: command_text,
            cwd: cwd.display().to_string(),
            family: reduction.family.clone(),
            canonical_kind: reduction.canonical_kind.clone(),
            exit_code,
            raw_est_tokens,
            reduced_est_tokens,
            estimate_scope: Some("rendered_command_body".to_string()),
            savings_percent: savings_pct,
            fallback_reason: None,
            failure_fingerprint,
            changed_paths,
            timestamp_unix_ms: timestamp_unix_ms(),
        },
    )?;
    if args.json {
        crate::cmd_common::emit_json(&payload, args.pretty)?;
    } else {
        print!("{rendered_command_body}");
        println!(
            "tokens: raw={raw_est_tokens} reduced={reduced_est_tokens} saved={saved} ({savings_pct:.1}%)"
        );
    }
    Ok(exit_code)
}

fn run_auto_fallback(
    root: &std::path::Path,
    cwd: &std::path::Path,
    args: &RunArgs,
    fallback_reason: &str,
) -> Result<i32> {
    if should_use_agent_runtime_backend(&args.command) {
        return run_platform_agent_backend(root, &args.command);
    }
    run_plain_command(
        root,
        cwd,
        &args.command,
        args.json,
        args.pretty,
        fallback_reason,
    )
}

fn should_use_agent_runtime_backend(argv: &[String]) -> bool {
    detect_agent_family(argv.first().map(String::as_str)) != "generic"
}

#[cfg(target_os = "macos")]
fn run_platform_agent_backend(root: &std::path::Path, argv: &[String]) -> Result<i32> {
    run_macos_swap(root, argv)
}

#[cfg(target_os = "linux")]
fn run_platform_agent_backend(root: &std::path::Path, argv: &[String]) -> Result<i32> {
    run_linux_preload(root, argv)
}

#[cfg(all(not(target_os = "macos"), not(target_os = "linux")))]
fn run_platform_agent_backend(_root: &std::path::Path, _argv: &[String]) -> Result<i32> {
    Err(anyhow!(
        "Packet28 run auto backend is not implemented for this platform"
    ))
}

fn run_plain_command(
    root: &std::path::Path,
    cwd: &std::path::Path,
    argv: &[String],
    json: bool,
    pretty: bool,
    fallback_reason: &str,
) -> Result<i32> {
    let command_text = command_text(argv);
    let before_changed_paths = current_changed_paths(root);
    let output = build_command(argv)?
        .output()
        .with_context(|| format!("failed to run `{command_text}`"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let exit_code = output.status.code().unwrap_or(1);
    if let Some(filter) =
        crate::toml_filters::apply_configured_filter(root, &command_text, &stdout, &stderr)?
    {
        return emit_filtered_run(FilteredRun {
            root,
            cwd,
            command_text: &command_text,
            exit_code,
            stdout: &stdout,
            stderr: &stderr,
            filter,
            before_changed_paths,
            json,
            pretty,
        });
    }
    let raw_est_tokens = estimate_tokens(&(stdout.clone() + &stderr));
    let raw_artifact_handle =
        write_run_raw_artifact(root, &command_text, exit_code, &stdout, &stderr)?;
    let failure_fingerprint = failure_fingerprint(exit_code, &stdout, &stderr);
    let changed_paths = new_changed_paths(root, &before_changed_paths);
    record_run_savings(
        root,
        &RunSavingsRecord {
            command: command_text.clone(),
            cwd: cwd.display().to_string(),
            family: "fallback".to_string(),
            canonical_kind: "raw_passthrough".to_string(),
            exit_code,
            raw_est_tokens,
            reduced_est_tokens: raw_est_tokens,
            estimate_scope: Some("rendered_command_body".to_string()),
            savings_percent: 0.0,
            fallback_reason: Some(fallback_reason.to_string()),
            failure_fingerprint: failure_fingerprint.clone(),
            changed_paths,
            timestamp_unix_ms: timestamp_unix_ms(),
        },
    )?;
    if json {
        crate::cmd_common::emit_json(
            &json!({
                "command": {
                    "original": command_text,
                    "exit_code": exit_code,
                    "timestamp_unix_ms": timestamp_unix_ms(),
                },
                "stdout": stdout,
                "stderr": stderr,
                "raw_est_tokens": raw_est_tokens,
                "reduced_est_tokens": raw_est_tokens,
                "estimate_scope": "rendered_command_body",
                "rendered_command_body": format!("{stdout}{stderr}"),
                "savings_percent": 0.0,
                "fallback_reason": fallback_reason,
                "failure_fingerprint": failure_fingerprint,
                "raw_artifact": {
                    "available": true,
                    "handle": raw_artifact_handle,
                },
            }),
            pretty,
        )?;
    } else {
        print!("{stdout}");
        eprint!("{stderr}");
    }
    Ok(exit_code)
}

struct FilteredRun<'a> {
    root: &'a std::path::Path,
    cwd: &'a std::path::Path,
    command_text: &'a str,
    exit_code: i32,
    stdout: &'a str,
    stderr: &'a str,
    filter: crate::toml_filters::AppliedTomlFilter,
    before_changed_paths: Vec<String>,
    json: bool,
    pretty: bool,
}

fn emit_filtered_run(run: FilteredRun<'_>) -> Result<i32> {
    let FilteredRun {
        root,
        cwd,
        command_text,
        exit_code,
        stdout,
        stderr,
        filter,
        before_changed_paths,
        json,
        pretty,
    } = run;
    let raw_est_tokens = estimate_tokens(&(stdout.to_string() + stderr));
    let rendered_command_body = format!("{}\n", filter.output);
    let reduced_est_tokens = estimate_tokens(&rendered_command_body);
    let saved = raw_est_tokens.saturating_sub(reduced_est_tokens);
    let savings_pct = if raw_est_tokens == 0 {
        0.0
    } else {
        (saved as f64 / raw_est_tokens as f64) * 100.0
    };
    let raw_artifact_handle =
        write_run_raw_artifact(root, command_text, exit_code, stdout, stderr)?;
    let failure_fingerprint = failure_fingerprint(exit_code, stdout, stderr);
    let payload = json!({
        "command": {
            "original": command_text,
            "cwd": cwd.display().to_string(),
            "exit_code": exit_code,
            "timestamp_unix_ms": timestamp_unix_ms(),
        },
        "estimate_scope": "rendered_command_body",
        "rendered_command_body": rendered_command_body,
        "reduction": {
            "family": "custom_filter",
            "canonical_kind": filter.name,
            "packet_type": "command.output.filter",
            "operation_kind": "generic",
            "summary": format!("custom TOML filter applied from {}", filter.source),
            "compact_preview": filter.output,
            "paths": [],
            "regions": [],
            "symbols": [],
            "metadata": {
                "source": filter.source,
                "filter_stderr": filter.filter_stderr,
            }
        },
        "raw_est_tokens": raw_est_tokens,
        "reduced_est_tokens": reduced_est_tokens,
        "savings_percent": savings_pct,
        "fallback_reason": null,
        "failure_fingerprint": failure_fingerprint,
        "raw_artifact": {
            "available": true,
            "handle": raw_artifact_handle,
        },
        "provenance": {
            "original_command": command_text,
            "cwd": cwd.display().to_string(),
            "exit_code": exit_code,
            "timestamp_unix_ms": timestamp_unix_ms(),
        }
    });
    record_run_savings(
        root,
        &RunSavingsRecord {
            command: command_text.to_string(),
            cwd: cwd.display().to_string(),
            family: "custom_filter".to_string(),
            canonical_kind: payload["reduction"]["canonical_kind"]
                .as_str()
                .unwrap_or("toml_filter")
                .to_string(),
            exit_code,
            raw_est_tokens,
            reduced_est_tokens,
            estimate_scope: Some("rendered_command_body".to_string()),
            savings_percent: savings_pct,
            fallback_reason: None,
            failure_fingerprint,
            changed_paths: new_changed_paths(root, &before_changed_paths),
            timestamp_unix_ms: timestamp_unix_ms(),
        },
    )?;
    if json {
        crate::cmd_common::emit_json(&payload, pretty)?;
    } else {
        print!("{rendered_command_body}");
        println!(
            "tokens: raw={raw_est_tokens} reduced={reduced_est_tokens} saved={saved} ({savings_pct:.1}%)"
        );
    }
    Ok(exit_code)
}

fn command_text(argv: &[String]) -> String {
    shell_words::join(argv.iter().map(String::as_str))
}

fn render_command_body(summary: &str, preview: &str) -> String {
    let mut body = String::new();
    if !summary.is_empty() {
        body.push_str(summary);
        body.push('\n');
    }
    if !preview.is_empty() && preview != summary {
        body.push_str(preview);
        body.push('\n');
    }
    body
}

fn estimate_tokens(value: &str) -> u64 {
    ((value.len() as f64) / 4.0).ceil() as u64
}

fn failure_fingerprint(exit_code: i32, stdout: &str, stderr: &str) -> Option<String> {
    if exit_code == 0 {
        return None;
    }
    let mut normalized = format!("exit={exit_code}\n");
    for line in stderr.lines().chain(stdout.lines()).take(20) {
        let compact = line
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase();
        if !compact.is_empty() {
            normalized.push_str(&compact);
            normalized.push('\n');
        }
    }
    let hash = blake3::hash(normalized.as_bytes()).to_hex().to_string();
    Some(format!("failure:v1:{}", &hash[..16]))
}

fn current_changed_paths(root: &std::path::Path) -> Vec<String> {
    let Ok(output) = Command::new("git")
        .current_dir(root)
        .args(["status", "--porcelain", "--untracked-files=all"])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut paths = stdout
        .lines()
        .filter_map(parse_git_status_path)
        .filter(|path| !is_packet28_telemetry_path(path))
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    paths
}

fn new_changed_paths(root: &std::path::Path, before: &[String]) -> Vec<String> {
    let before = before.iter().collect::<std::collections::HashSet<_>>();
    current_changed_paths(root)
        .into_iter()
        .filter(|path| !before.contains(path))
        .collect()
}

fn parse_git_status_path(line: &str) -> Option<String> {
    let path = line.get(3..)?.trim();
    if path.is_empty() {
        return None;
    }
    let path = path
        .rsplit_once(" -> ")
        .map(|(_, new_path)| new_path)
        .unwrap_or(path)
        .trim_matches('"');
    (!path.is_empty()).then(|| path.to_string())
}

fn is_packet28_telemetry_path(path: &str) -> bool {
    path.starts_with(".packet28/") || path.starts_with(".covy/state/")
}

fn write_run_raw_artifact(
    root: &Path,
    command_text: &str,
    exit_code: i32,
    stdout: &str,
    stderr: &str,
) -> Result<String> {
    let dir = root.join(".packet28").join("run-raw");
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create run raw artifact dir '{}'", dir.display()))?;
    let file_name = format!(
        "{}-{}.txt",
        timestamp_unix_ms(),
        raw_artifact_slug(command_text)
    );
    let path = dir.join(file_name);
    fs::write(
        &path,
        format!(
            "command: {command_text}\nexit_code: {exit_code}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
        ),
    )
    .with_context(|| format!("failed to write raw artifact '{}'", path.display()))?;
    Ok(path
        .strip_prefix(root)
        .unwrap_or(&path)
        .display()
        .to_string())
}

fn raw_artifact_slug(command_text: &str) -> String {
    let slug = command_text
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    slug.trim_matches('-').chars().take(48).collect()
}

fn timestamp_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

#[cfg(target_os = "linux")]
fn run_linux_preload(root: &std::path::Path, argv: &[String]) -> Result<i32> {
    crate::cmd_shell::launch_linux_preload(root, argv, RuntimeBackend::LinuxPreload.as_env_value())
}

#[cfg(not(target_os = "linux"))]
fn run_linux_preload(_root: &std::path::Path, _argv: &[String]) -> Result<i32> {
    Err(anyhow!(
        "Packet28 run --backend linux-preload is only available on Linux"
    ))
}

#[cfg(target_os = "macos")]
fn run_macos_swap(root: &std::path::Path, argv: &[String]) -> Result<i32> {
    crate::cmd_macos_swap::launch_macos_swap(root, argv, RuntimeBackend::MacosSwap.as_env_value())
}

#[cfg(not(target_os = "macos"))]
fn run_macos_swap(root: &std::path::Path, argv: &[String]) -> Result<i32> {
    crate::cmd_macos_swap::launch_macos_swap(root, argv, RuntimeBackend::MacosSwap.as_env_value())
}

fn run_linux_oci(root: &std::path::Path, argv: &[String]) -> Result<i32> {
    crate::cmd_daemon::ensure_daemon(root)?;
    if std::env::var_os("PACKET28_ENABLE_EXPERIMENTAL_OCI").is_none() {
        return Err(anyhow!(
            "Packet28 run --backend linux-oci is not implemented yet; use a Linux host with --backend linux-preload today"
        ));
    }
    run_passthrough(root, argv, RuntimeBackend::LinuxOci)
}

fn run_macos_fuse(root: &std::path::Path, argv: &[String]) -> Result<i32> {
    crate::cmd_daemon::ensure_daemon(root)?;
    if std::env::var_os("PACKET28_ENABLE_EXPERIMENTAL_MACOS_FUSE").is_none() {
        return Err(anyhow!(
            "Packet28 run --backend macos-fuse is not implemented yet; use --backend macos-swap as the current macOS backend"
        ));
    }
    run_passthrough(root, argv, RuntimeBackend::MacosFuse)
}

fn run_windows_fuse(root: &std::path::Path, argv: &[String]) -> Result<i32> {
    crate::cmd_daemon::ensure_daemon(root)?;
    if std::env::var_os("PACKET28_ENABLE_EXPERIMENTAL_WINDOWS_FUSE").is_none() {
        return Err(anyhow!(
            "Packet28 run --backend windows-fuse is not implemented yet; use --backend linux-oci when container fallback is available"
        ));
    }
    run_passthrough(root, argv, RuntimeBackend::WindowsFuse)
}

fn run_proxy_only(root: &std::path::Path, argv: &[String]) -> Result<i32> {
    crate::cmd_daemon::ensure_daemon(root)?;
    if std::env::var_os("PACKET28_ENABLE_EXPERIMENTAL_PROXY_ONLY").is_none() {
        return Err(anyhow!(
            "Packet28 run --backend proxy-only is not implemented yet; the local HTTP proxy backend has not been added"
        ));
    }
    run_passthrough(root, argv, RuntimeBackend::ProxyOnly)
}

fn run_passthrough(
    root: &std::path::Path,
    argv: &[String],
    backend: RuntimeBackend,
) -> Result<i32> {
    let mut command = build_command(argv)?;
    command.env("PACKET28_DAEMON_ROOT", root);
    command.env("PACKET28_RUNTIME_BACKEND", backend.as_env_value());
    command.env(
        "PACKET28_AGENT_FAMILY",
        detect_agent_family(argv.first().map(String::as_str)),
    );
    let status = command.status().with_context(|| {
        format!(
            "failed to launch runtime backend {}",
            backend.as_env_value()
        )
    })?;
    Ok(status.code().unwrap_or(1))
}

fn build_command(argv: &[String]) -> Result<Command> {
    let Some(program) = argv.first() else {
        return Err(anyhow!("Packet28 run requires a command after --"));
    };
    let mut command = Command::new(program);
    command.args(&argv[1..]);
    Ok(command)
}

fn detect_agent_family(program: Option<&str>) -> String {
    let Some(program) = program else {
        return "generic".to_string();
    };
    let lower = std::path::Path::new(program)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(program)
        .to_ascii_lowercase();
    if lower.contains("claude") {
        "claude".to_string()
    } else if lower.contains("codex") {
        "codex".to_string()
    } else if lower.contains("cursor") {
        "cursor".to_string()
    } else if lower.contains("opencode") {
        "opencode".to_string()
    } else {
        "generic".to_string()
    }
}
