use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use packet28_daemon_core::storage::{load_task_registry, now_unix};
use packet28_daemon_core::task_store_lease::acquire_task_store_writer_lease;
use packet28_daemon_protocol::hooks::{
    ActiveTaskRecord, HookBoundaryKind, HookEventKind, HookIngestRequest, HookLifecycleEvent,
    HookLifecycleKind, HookReducerCacheEntry, HookReducerPacket,
};
use packet28_daemon_protocol::paths::{task_artifact_dir, TaskStorageId};
use packet28_daemon_protocol::task::TaskRecord;
use packet28_reducer_core::{
    classify_command, classify_command_argv, reduce_command_output, CommandReducerSpec,
};
use serde_json::json;

use crate::cmd_hook::{
    compact_text, estimate_text_tokens, now_unix_millis, payload_text_len, reduction_pct,
    shell_join, ReduceFixtureArgs, ReducerRunnerArgs,
};

pub(crate) fn run_reducer_runner(args: ReducerRunnerArgs) -> Result<i32> {
    let root = crate::broker_client::resolve_root(&args.root);
    crate::broker_client::ensure_daemon(&root)?;
    let _writer_lease = acquire_task_store_writer_lease(&root)?;
    if args.argv.is_empty() {
        return Err(anyhow!("reducer-runner requires a command after '--'"));
    }

    let task_id = if let Some(task_id) = args.task_id.clone() {
        task_id
    } else if let Some(active) = crate::task_runtime::load_active_task(&root)? {
        active.task_id
    } else {
        crate::broker_client::derive_task_id("claude-hook-runner")
    };
    let task_id = crate::task_runtime::resolve_task_continuation(&root, &task_id)?;
    let task_storage_id = TaskStorageId::try_from(task_id.as_str())?;
    crate::task_runtime::store_active_task(
        &root,
        &ActiveTaskRecord {
            task_id: task_id.clone(),
            session_id: args.session_id.clone(),
            updated_at_unix: now_unix(),
        },
    )?;

    let cwd = args
        .cwd
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| root.clone());
    let command_text = shell_join(&args.argv);
    let spec = classify_command_argv(&command_text, &args.argv)
        .ok_or_else(|| anyhow!("command is not eligible for reducer rewrite"))?;
    if spec.family != args.family
        || spec.canonical_kind != args.kind
        || spec.cache_fingerprint != args.fingerprint
    {
        return Err(anyhow!("reducer-runner classification mismatch"));
    }

    let workspace_fingerprint = workspace_cache_fingerprint(&root, &cwd, &spec);

    if let Some((cached_packet, exit_code)) = cached_reducer_packet(
        &root,
        &task_id,
        &spec,
        &command_text,
        Some(&workspace_fingerprint),
    ) {
        let command_id = format!("runner-cache-{}", now_unix_millis());
        let _ = crate::broker_client::hook_ingest(
            &root,
            HookIngestRequest {
                task_id,
                session_id: args.session_id,
                event_kind: HookEventKind::CommandFinished,
                matcher: None,
                source: Some("packet28-reducer-runner-cache".to_string()),
                boundary_kind: HookBoundaryKind::None,
                lifecycle_event: Some(HookLifecycleEvent {
                    kind: HookLifecycleKind::CommandFinished,
                    command_id: Some(command_id),
                    reducer_family: cached_packet.reducer_family.clone(),
                    canonical_command_kind: cached_packet.canonical_command_kind.clone(),
                    cache_fingerprint: cached_packet.cache_fingerprint.clone(),
                    elapsed_ms: Some(0),
                    exit_code: cached_packet.exit_code,
                    ..HookLifecycleEvent::default()
                }),
                reducer_packet: Some(cached_packet.clone()),
                host_context_budget_tokens: None,
            },
        )?;
        println!("{}", cached_packet.summary);
        return Ok(exit_code);
    }

    let command_id = format!("runner-{}", now_unix_millis());
    let spool_dir = task_artifact_dir(&root, &task_storage_id).join("hook-spool");
    let stdout_path = spool_dir.join(format!("{command_id}-stdout.log"));
    let stderr_path = spool_dir.join(format!("{command_id}-stderr.log"));

    let admission = crate::broker_client::hook_ingest(
        &root,
        HookIngestRequest {
            task_id: task_id.clone(),
            session_id: args.session_id.clone(),
            event_kind: HookEventKind::CommandStarted,
            matcher: None,
            source: Some("packet28-reducer-runner".to_string()),
            boundary_kind: HookBoundaryKind::None,
            lifecycle_event: Some(HookLifecycleEvent {
                kind: HookLifecycleKind::CommandStarted,
                command_id: Some(command_id.clone()),
                reducer_family: Some(spec.family.clone()),
                canonical_command_kind: Some(spec.canonical_kind.clone()),
                cache_fingerprint: Some(spec.cache_fingerprint.clone()),
                stdout_spool_path: Some(stdout_path.display().to_string()),
                stderr_spool_path: Some(stderr_path.display().to_string()),
                ..HookLifecycleEvent::default()
            }),
            reducer_packet: None,
            host_context_budget_tokens: None,
        },
    )?;
    if !admission.accepted {
        return Err(anyhow!(
            "reducer-runner task admission was rejected for '{task_id}'"
        ));
    }

    fs::create_dir_all(&spool_dir)?;
    let stdout_file = File::create(&stdout_path)
        .with_context(|| format!("failed to create '{}'", stdout_path.display()))?;
    let stderr_file = File::create(&stderr_path)
        .with_context(|| format!("failed to create '{}'", stderr_path.display()))?;

    let started = Instant::now();
    let mut child = Command::new(&args.argv[0])
        .args(&args.argv[1..])
        .current_dir(&cwd)
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .envs(args.env.iter().filter_map(|entry| {
            entry
                .split_once('=')
                .map(|(key, value)| (key.to_string(), value.to_string()))
        }))
        .spawn()
        .with_context(|| format!("failed to spawn '{}'", args.argv[0]))?;

    let mut last_stdout_bytes = 0_u64;
    let mut last_stderr_bytes = 0_u64;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        let stdout_bytes = fs::metadata(&stdout_path)
            .map(|meta| meta.len())
            .unwrap_or(last_stdout_bytes);
        let stderr_bytes = fs::metadata(&stderr_path)
            .map(|meta| meta.len())
            .unwrap_or(last_stderr_bytes);
        if stdout_bytes != last_stdout_bytes || stderr_bytes != last_stderr_bytes {
            last_stdout_bytes = stdout_bytes;
            last_stderr_bytes = stderr_bytes;
            let _ = crate::broker_client::hook_ingest(
                &root,
                HookIngestRequest {
                    task_id: task_id.clone(),
                    session_id: args.session_id.clone(),
                    event_kind: HookEventKind::CommandProgress,
                    matcher: None,
                    source: Some("packet28-reducer-runner".to_string()),
                    boundary_kind: HookBoundaryKind::None,
                    lifecycle_event: Some(HookLifecycleEvent {
                        kind: HookLifecycleKind::CommandProgress,
                        command_id: Some(command_id.clone()),
                        reducer_family: Some(spec.family.clone()),
                        canonical_command_kind: Some(spec.canonical_kind.clone()),
                        cache_fingerprint: Some(spec.cache_fingerprint.clone()),
                        stdout_spool_path: Some(stdout_path.display().to_string()),
                        stderr_spool_path: Some(stderr_path.display().to_string()),
                        stdout_bytes: Some(stdout_bytes),
                        stderr_bytes: Some(stderr_bytes),
                        elapsed_ms: Some(started.elapsed().as_millis() as u64),
                        ..HookLifecycleEvent::default()
                    }),
                    reducer_packet: None,
                    host_context_budget_tokens: None,
                },
            );
        }
        thread::sleep(Duration::from_millis(200));
    };

    let stdout = read_to_string_lossy(&stdout_path).unwrap_or_default();
    let stderr = read_to_string_lossy(&stderr_path).unwrap_or_default();
    let exit_code = status.code().unwrap_or(1);
    let reduced = reduce_command_output(&spec, &stdout, &stderr, exit_code)?;
    let artifact = json!({
        "command_id": command_id,
        "command": command_text,
        "argv": args.argv,
        "cwd": cwd.display().to_string(),
        "cache_hit": false,
        "cache_validity": "workspace_fingerprint",
        "workspace_fingerprint": workspace_fingerprint,
        "stdout_spool_path": stdout_path.display().to_string(),
        "stderr_spool_path": stderr_path.display().to_string(),
        "stdout_preview": compact_text(&stdout, 400),
        "stderr_preview": compact_text(&stderr, 400),
        "stdout_bytes": fs::metadata(&stdout_path).map(|meta| meta.len()).unwrap_or(0),
        "stderr_bytes": fs::metadata(&stderr_path).map(|meta| meta.len()).unwrap_or(0),
        "exit_code": exit_code,
    });
    let est_bytes = reduced.summary.len() as u64;
    let est_tokens = ((est_bytes as f64) / 4.0).ceil() as u64;
    let response = crate::broker_client::hook_ingest(
        &root,
        HookIngestRequest {
            task_id,
            session_id: args.session_id,
            event_kind: HookEventKind::CommandFinished,
            matcher: None,
            source: Some("packet28-reducer-runner".to_string()),
            boundary_kind: HookBoundaryKind::None,
            lifecycle_event: Some(HookLifecycleEvent {
                kind: HookLifecycleKind::CommandFinished,
                command_id: Some(command_id),
                reducer_family: Some(reduced.family.clone()),
                canonical_command_kind: Some(reduced.canonical_kind.clone()),
                cache_fingerprint: Some(reduced.cache_fingerprint.clone()),
                stdout_spool_path: Some(stdout_path.display().to_string()),
                stderr_spool_path: Some(stderr_path.display().to_string()),
                stdout_bytes: Some(
                    fs::metadata(&stdout_path)
                        .map(|meta| meta.len())
                        .unwrap_or(0),
                ),
                stderr_bytes: Some(
                    fs::metadata(&stderr_path)
                        .map(|meta| meta.len())
                        .unwrap_or(0),
                ),
                elapsed_ms: Some(started.elapsed().as_millis() as u64),
                exit_code: Some(exit_code),
            }),
            reducer_packet: Some(HookReducerPacket {
                packet_type: reduced.packet_type,
                tool_name: "Bash".to_string(),
                operation_kind: reduced.operation_kind,
                reducer_family: Some(reduced.family),
                canonical_command_kind: Some(reduced.canonical_kind),
                summary: reduced.summary.clone(),
                compact_preview: (!reduced.compact_preview.is_empty())
                    .then_some(reduced.compact_preview.clone()),
                command: Some(command_text),
                search_query: None,
                compact_path: Some("reducer_rewrite".to_string()),
                passthrough_reason: None,
                raw_est_tokens: Some((((stdout.len() + stderr.len()) as f64) / 4.0).ceil() as u64),
                reduced_est_tokens: Some(est_tokens),
                paths: reduced.paths,
                regions: reduced.regions,
                symbols: reduced.symbols,
                equivalence_key: reduced.equivalence_key,
                est_tokens,
                est_bytes,
                failed: reduced.failed,
                error_class: reduced.error_class,
                error_message: reduced.error_message,
                retryable: reduced.retryable,
                duration_ms: Some(started.elapsed().as_millis() as u64),
                exit_code: Some(reduced.exit_code),
                cache_fingerprint: Some(reduced.cache_fingerprint),
                cacheable: Some(reduced.cacheable),
                mutation: Some(reduced.mutation),
                raw_artifact_handle: stdout_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_owned),
                raw_artifact_available: true,
                artifact: Some(artifact),
            }),
            host_context_budget_tokens: None,
        },
    )?;
    let _ = response;
    println!("{}", reduced.summary);
    Ok(exit_code)
}

pub(crate) fn run_reduce_fixture(args: ReduceFixtureArgs) -> Result<i32> {
    let stdout = fs::read_to_string(&args.stdout_path)
        .with_context(|| format!("failed to read fixture '{}'", args.stdout_path))?;
    let stderr = if let Some(stderr_path) = args.stderr_path.as_ref() {
        fs::read_to_string(stderr_path)
            .with_context(|| format!("failed to read fixture '{stderr_path}'"))?
    } else {
        String::new()
    };
    let spec = classify_command(&args.command)
        .ok_or_else(|| anyhow!("fixture command is not eligible for reducer classification"))?;
    let reduced = reduce_command_output(&spec, &stdout, &stderr, args.exit_code)?;
    let raw_visible = format!("{stdout}{stderr}");
    let raw_tokens = estimate_text_tokens(&raw_visible);
    let reduced_tokens = estimate_text_tokens(&reduced.summary);
    let payload = json!({
        "command": args.command,
        "family": reduced.family,
        "canonical_kind": reduced.canonical_kind,
        "summary": reduced.summary,
        "failed": reduced.failed,
        "exit_code": reduced.exit_code,
        "raw_bytes": raw_visible.len(),
        "raw_est_tokens": raw_tokens,
        "reduced_bytes": payload_text_len(&reduced.summary),
        "reduced_est_tokens": reduced_tokens,
        "raw_preview": compact_text(&raw_visible, 400),
        "reduced_preview": reduced.summary,
        "token_reduction_pct": reduction_pct(raw_tokens, reduced_tokens),
    });
    if args.json {
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        println!(
            "{}",
            payload["reduced_preview"].as_str().unwrap_or_default()
        );
    }
    Ok(0)
}

pub(crate) fn workspace_cache_fingerprint(
    root: &Path,
    cwd: &Path,
    spec: &CommandReducerSpec,
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"packet28-workspace-cache-v1");
    hash_path_component(&mut hasher, "root", root);
    hash_path_component(&mut hasher, "cwd", cwd);
    hasher.update(spec.family.as_bytes());
    hasher.update(spec.canonical_kind.as_bytes());

    let mut paths = workspace_fingerprint_paths(root, cwd, spec);
    paths.sort();
    paths.dedup();

    for args in [
        &["rev-parse", "--show-toplevel"][..],
        &["rev-parse", "HEAD"][..],
    ] {
        match git_output_for_fingerprint(root, args) {
            Some(output) => {
                hasher.update(b"git-ok");
                hasher.update(output.as_bytes());
            }
            None => {
                hasher.update(b"git-unavailable");
            }
        }
    }
    if paths.is_empty() {
        match git_output_for_fingerprint(
            root,
            &["status", "--porcelain=v1", "--untracked-files=all"],
        ) {
            Some(output) => {
                hasher.update(b"git-status-ok");
                hasher.update(output.as_bytes());
            }
            None => {
                hasher.update(b"git-status-unavailable");
            }
        }
    }
    for path in paths {
        hash_file_for_fingerprint(&mut hasher, root, &path);
    }

    hasher.finalize().to_hex().to_string()
}

fn workspace_fingerprint_paths(root: &Path, cwd: &Path, spec: &CommandReducerSpec) -> Vec<PathBuf> {
    if spec.family == "rust" {
        let base = if cwd.exists() { cwd } else { root };
        let mut paths = Vec::new();
        collect_rust_workspace_paths(base, &mut paths);
        paths
    } else if !spec.paths.is_empty() {
        spec.paths
            .iter()
            .map(|path| {
                let candidate = PathBuf::from(path);
                if candidate.is_absolute() {
                    candidate
                } else {
                    cwd.join(candidate)
                }
            })
            .collect()
    } else {
        Vec::new()
    }
}

fn collect_rust_workspace_paths(dir: &Path, paths: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        if file_type.is_dir() {
            if matches!(name, ".git" | ".packet28" | "target" | "node_modules") {
                continue;
            }
            collect_rust_workspace_paths(&path, paths);
        } else if file_type.is_file()
            && (path.extension().and_then(|value| value.to_str()) == Some("rs")
                || matches!(name, "Cargo.toml" | "Cargo.lock"))
        {
            paths.push(path);
        }
    }
}

fn hash_file_for_fingerprint(hasher: &mut blake3::Hasher, root: &Path, path: &Path) {
    let display_path = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string();
    hasher.update(display_path.as_bytes());
    match fs::metadata(path) {
        Ok(metadata) => {
            hasher.update(b"exists");
            hasher.update(&metadata.len().to_le_bytes());
            if let Ok(bytes) = fs::read(path) {
                hasher.update(&bytes);
            }
        }
        Err(_) => {
            hasher.update(b"missing");
        }
    }
}

fn hash_path_component(hasher: &mut blake3::Hasher, label: &str, path: &Path) {
    hasher.update(label.as_bytes());
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    hasher.update(path.to_string_lossy().as_bytes());
}

fn git_output_for_fingerprint(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

fn cached_reducer_packet(
    root: &Path,
    task_id: &str,
    spec: &CommandReducerSpec,
    command_text: &str,
    workspace_fingerprint: Option<&str>,
) -> Option<(HookReducerPacket, i32)> {
    if spec.mutation {
        return None;
    }
    let registry = load_task_registry(root).ok()?;
    let task = registry.tasks.get(task_id)?;
    let entry = task.hook_reducer_cache.get(&spec.cache_fingerprint)?;
    if !cache_entry_matches(task, entry, spec, workspace_fingerprint) {
        return None;
    }
    let est_bytes = entry.summary.len() as u64;
    let est_tokens = ((est_bytes as f64) / 4.0).ceil() as u64;
    let exit_code = entry.exit_code.unwrap_or(if entry.failed { 1 } else { 0 });
    Some((
        HookReducerPacket {
            packet_type: spec.packet_type.clone(),
            tool_name: "Bash".to_string(),
            operation_kind: spec.operation_kind,
            reducer_family: Some(spec.family.clone()),
            canonical_command_kind: Some(spec.canonical_kind.clone()),
            summary: entry.summary.clone(),
            compact_preview: entry.compact_preview.clone(),
            command: Some(command_text.to_string()),
            search_query: None,
            compact_path: Some("reducer_rewrite".to_string()),
            passthrough_reason: None,
            raw_est_tokens: None,
            reduced_est_tokens: Some(est_tokens),
            paths: entry.paths.clone(),
            regions: entry.regions.clone(),
            symbols: entry.symbols.clone(),
            equivalence_key: spec.equivalence_key.clone(),
            est_tokens,
            est_bytes,
            failed: entry.failed,
            error_class: entry.failed.then_some("cached_tool_error".to_string()),
            error_message: entry.error_message.clone(),
            retryable: entry.failed.then_some(false),
            duration_ms: Some(0),
            exit_code: Some(exit_code),
            cache_fingerprint: Some(spec.cache_fingerprint.clone()),
            cacheable: Some(spec.cacheable),
            mutation: Some(spec.mutation),
            raw_artifact_handle: entry.raw_artifact_handle.clone(),
            raw_artifact_available: entry.raw_artifact_handle.is_some(),
            artifact: None,
        },
        exit_code,
    ))
}

fn cache_entry_matches(
    task: &TaskRecord,
    entry: &HookReducerCacheEntry,
    spec: &CommandReducerSpec,
    workspace_fingerprint: Option<&str>,
) -> bool {
    if spec.mutation {
        return false;
    }
    if entry.reducer_family != spec.family || entry.canonical_command_kind != spec.canonical_kind {
        return false;
    }
    if workspace_fingerprint.is_some()
        && entry.workspace_fingerprint.as_deref() != workspace_fingerprint
    {
        return false;
    }
    if entry.git_epoch != task.hook_git_epoch
        || entry.fs_epoch != task.hook_fs_epoch
        || entry.rust_epoch != task.hook_rust_epoch
    {
        return false;
    }
    if let Some(ttl_secs) =
        remote_state_cache_ttl_secs(&entry.reducer_family, &entry.canonical_command_kind)
    {
        let age = now_unix().saturating_sub(entry.occurred_at_unix);
        return age <= ttl_secs;
    }
    true
}

fn read_to_string_lossy(path: &Path) -> std::io::Result<String> {
    fs::read(path).map(|bytes| String::from_utf8_lossy(&bytes).to_string())
}

fn remote_state_cache_ttl_secs(family: &str, kind: &str) -> Option<u64> {
    match family {
        "github" => Some(300),
        "infra"
            if kind.starts_with("aws_")
                || kind == "psql_query"
                || kind.starts_with("docker_")
                || kind.starts_with("docker_compose_")
                || kind.starts_with("kubectl_")
                || kind == "curl_fetch" =>
        {
            Some(300)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_non_utf8_output_lossily() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stdout.bin");
        fs::write(&path, [b'o', b'k', 0xff, b'\n']).unwrap();

        let text = read_to_string_lossy(&path).unwrap();
        assert_eq!(text, "ok\u{fffd}\n");
    }
}
