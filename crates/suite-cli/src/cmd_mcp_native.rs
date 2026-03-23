use super::*;
use crate::cmd_mcp::support::{
    load_raw_output_artifact, load_tool_result_artifact, next_task_invocation,
    store_result_artifact, write_auto_capture_state_batch_via_session,
};
use glob::Pattern;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Packet28SearchResponseMode {
    #[default]
    Slim,
    Full,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub(crate) struct Packet28SearchArgs {
    pub(crate) task_id: String,
    pub(crate) query: String,
    pub(crate) paths: Vec<String>,
    pub(crate) fixed_string: bool,
    pub(crate) case_sensitive: Option<bool>,
    pub(crate) whole_word: bool,
    pub(crate) context_lines: Option<usize>,
    pub(crate) max_matches_per_file: Option<usize>,
    pub(crate) max_total_matches: Option<usize>,
    pub(crate) response_mode: Packet28SearchResponseMode,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub(crate) struct Packet28ReadRegionsArgs {
    pub(crate) task_id: String,
    pub(crate) path: String,
    pub(crate) regions: Vec<String>,
    pub(crate) line_start: Option<usize>,
    pub(crate) line_end: Option<usize>,
    pub(crate) response_mode: Packet28SearchResponseMode,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub(crate) struct Packet28GlobArgs {
    pub(crate) task_id: String,
    pub(crate) pattern: String,
    pub(crate) paths: Vec<String>,
    pub(crate) max_results: Option<usize>,
    pub(crate) response_mode: Packet28SearchResponseMode,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub(crate) struct Packet28FetchToolResultArgs {
    pub(crate) task_id: String,
    pub(crate) artifact_id: Option<String>,
    pub(crate) invocation_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub(crate) struct Packet28FetchRawOutputArgs {
    pub(crate) task_id: String,
    pub(crate) handle: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub(crate) struct Packet28FetchContextArgs {
    pub(crate) task_id: String,
    pub(crate) artifact_id: Option<String>,
    pub(crate) context_version: Option<String>,
    pub(crate) response_mode: Option<BrokerResponseMode>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub(crate) struct Packet28PrepareHandoffArgs {
    pub(crate) task_id: String,
    pub(crate) query: Option<String>,
    pub(crate) response_mode: Option<BrokerResponseMode>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub(crate) struct Packet28WriteIntentionArgs {
    pub(crate) task_id: String,
    pub(crate) text: String,
    pub(crate) note: Option<String>,
    pub(crate) step_id: Option<String>,
    pub(crate) question_id: Option<String>,
    pub(crate) paths: Vec<String>,
    pub(crate) symbols: Vec<String>,
}

fn json_array_strings(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.as_str().map(ToOwned::to_owned))
        .collect()
}

fn search_request_summary(args: &Packet28SearchArgs) -> String {
    if args.paths.is_empty() {
        format!(
            "search '{}' across repo ({:?})",
            args.query, args.response_mode
        )
    } else {
        format!(
            "search '{}' in {} path(s) ({:?})",
            args.query,
            args.paths.len(),
            args.response_mode
        )
    }
}

fn read_regions_request_summary(args: &Packet28ReadRegionsArgs, path: &str) -> String {
    if !args.regions.is_empty() {
        format!(
            "read_regions {path} using {} region hint(s)",
            args.regions.len()
        )
    } else if args.line_start.is_some() || args.line_end.is_some() {
        format!(
            "read_regions {path} lines {}-{}",
            args.line_start.unwrap_or(1),
            args.line_end.unwrap_or(args.line_start.unwrap_or(1))
        )
    } else {
        format!("read_regions {path}")
    }
}

fn glob_request_summary(args: &Packet28GlobArgs) -> String {
    if args.paths.is_empty() {
        format!(
            "glob '{}' across repo ({:?})",
            args.pattern, args.response_mode
        )
    } else {
        format!(
            "glob '{}' in {} path(s) ({:?})",
            args.pattern,
            args.paths.len(),
            args.response_mode
        )
    }
}

fn estimate_tokens_for_value(value: &Value) -> u64 {
    let bytes = serde_json::to_vec(value).unwrap_or_default().len() as f64;
    (bytes / 4.0).ceil() as u64
}

fn write_native_tool_result(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    task_id: &str,
    invocation_id: &str,
    sequence: u64,
    tool_name: &str,
    operation_kind: suite_packet_core::ToolOperationKind,
    request_summary: String,
    result_summary: String,
    compact_path: &str,
    raw_est_tokens: Option<u64>,
    reduced_est_tokens: Option<u64>,
    search_query: Option<String>,
    command: Option<String>,
    paths: Vec<String>,
    regions: Vec<String>,
    symbols: Vec<String>,
    artifact_id: Option<String>,
    raw_artifact_handle: Option<String>,
    duration_ms: u64,
) -> Result<()> {
    write_auto_capture_state_batch_via_session(
        root,
        session,
        vec![BrokerWriteStateRequest {
            task_id: task_id.to_string(),
            op: Some(BrokerWriteOp::ToolResult),
            invocation_id: Some(invocation_id.to_string()),
            tool_name: Some(tool_name.to_string()),
            operation_kind: Some(operation_kind),
            request_summary: Some(request_summary),
            result_summary: Some(result_summary),
            compact_path: Some(compact_path.to_string()),
            raw_est_tokens,
            reduced_est_tokens,
            search_query,
            command,
            sequence: Some(sequence),
            duration_ms: Some(duration_ms),
            paths,
            regions,
            symbols,
            artifact_id,
            raw_artifact_handle: raw_artifact_handle.clone(),
            raw_artifact_available: Some(raw_artifact_handle.is_some()),
            refresh_context: Some(false),
            ..BrokerWriteStateRequest::default()
        }],
    )
}

fn write_native_tool_failure(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    task_id: &str,
    invocation_id: &str,
    sequence: u64,
    tool_name: &str,
    operation_kind: suite_packet_core::ToolOperationKind,
    request_summary: String,
    error_message: String,
    compact_path: &str,
    raw_est_tokens: Option<u64>,
    reduced_est_tokens: Option<u64>,
    command: Option<String>,
    raw_artifact_handle: Option<String>,
    duration_ms: u64,
) -> Result<()> {
    write_auto_capture_state_batch_via_session(
        root,
        session,
        vec![BrokerWriteStateRequest {
            task_id: task_id.to_string(),
            op: Some(BrokerWriteOp::ToolInvocationFailed),
            invocation_id: Some(invocation_id.to_string()),
            tool_name: Some(tool_name.to_string()),
            operation_kind: Some(operation_kind),
            request_summary: Some(request_summary),
            compact_path: Some(compact_path.to_string()),
            error_class: Some(classify_error_message(&error_message)),
            error_message: Some(error_message.clone()),
            raw_est_tokens,
            reduced_est_tokens,
            retryable: Some(is_retryable_error(&error_message)),
            sequence: Some(sequence),
            duration_ms: Some(duration_ms),
            command,
            raw_artifact_handle: raw_artifact_handle.clone(),
            raw_artifact_available: Some(raw_artifact_handle.is_some()),
            refresh_context: Some(false),
            ..BrokerWriteStateRequest::default()
        }],
    )
}

pub(crate) fn handle_packet28_search(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    args: Packet28SearchArgs,
) -> Result<Value> {
    let task_id = args.task_id.trim();
    if task_id.is_empty() {
        return Err(anyhow!("packet28.search requires task_id"));
    }
    let query = args.query.trim();
    if query.is_empty() {
        return Err(anyhow!("packet28.search requires query"));
    }
    let (sequence, invocation_id) = next_task_invocation(session, task_id)?;
    let request_summary = search_request_summary(&args);

    let started_at = Instant::now();
    let search_result = match packet28_reducer_core::search(
        root,
        &packet28_reducer_core::SearchRequest {
            query: query.to_string(),
            requested_paths: args.paths.clone(),
            fixed_string: args.fixed_string,
            case_sensitive: args.case_sensitive,
            whole_word: args.whole_word,
            context_lines: args.context_lines,
            max_matches_per_file: args.max_matches_per_file,
            max_total_matches: args.max_total_matches,
        },
    ) {
        Ok(result) => result,
        Err(error) => {
            let duration_ms = started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
            write_native_tool_failure(
                root,
                session,
                task_id,
                &invocation_id,
                sequence,
                "packet28.search",
                suite_packet_core::ToolOperationKind::Search,
                request_summary,
                error.to_string(),
                "native_tool",
                None,
                None,
                None,
                None,
                duration_ms,
            )?;
            return Err(error);
        }
    };
    let duration_ms = started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    let groups = search_result
        .groups
        .iter()
        .map(|group| {
            json!({
                "path": group.path,
                "match_count": group.match_count,
                "displayed_match_count": group.displayed_match_count,
                "truncated": group.truncated,
                "matches": group.matches,
            })
        })
        .collect::<Vec<_>>();
    let result_summary = if search_result.match_count == 0 {
        if !args.paths.is_empty() && search_result.resolved_paths.is_empty() {
            format!(
                "No search paths resolved for '{}' ({} requested path(s) missing)",
                query,
                args.paths.len()
            )
        } else if search_result.resolved_paths.is_empty() {
            format!("No matches for '{}' across repo", query)
        } else {
            format!(
                "No matches for '{}' in {} path(s)",
                query,
                search_result.resolved_paths.len()
            )
        }
    } else {
        search_result
            .compact_preview
            .lines()
            .next()
            .unwrap_or("Search completed")
            .to_string()
    };
    let requested_paths = search_result.requested_paths.clone();
    let resolved_paths = search_result.resolved_paths.clone();
    let paths = search_result.paths.clone();
    let regions = search_result.regions.clone();
    let symbols = search_result.symbols.clone();
    let compact_preview = search_result.compact_preview.clone();
    let diagnostics = search_result.diagnostics.clone();
    let full_payload = json!({
        "task_id": task_id,
        "invocation_id": invocation_id,
        "sequence": sequence,
        "query": query,
        "match_count": search_result.match_count,
        "returned_match_count": search_result.returned_match_count,
        "truncated": search_result.truncated,
        "requested_paths": requested_paths,
        "resolved_paths": resolved_paths,
        "paths": paths.clone(),
        "regions": regions.clone(),
        "symbols": symbols.clone(),
        "groups": groups,
        "compact_preview": compact_preview,
        "diagnostics": diagnostics,
        "response_mode": "full",
    });
    let artifact_id = Some(store_result_artifact(
        root,
        task_id,
        full_payload["invocation_id"].as_str().unwrap_or_default(),
        &full_payload,
    )?);
    let payload = match args.response_mode {
        Packet28SearchResponseMode::Full => {
            let mut payload = full_payload.clone();
            payload["artifact_id"] = json!(artifact_id.clone());
            payload
        }
        Packet28SearchResponseMode::Slim => json!({
            "match_count": search_result.match_count,
            "compact_preview": full_payload["compact_preview"].clone(),
            "artifact_id": artifact_id.clone(),
            "response_mode": "slim",
        }),
    };
    let raw_est_tokens = Some(estimate_tokens_for_value(&full_payload));
    let reduced_est_tokens = Some(estimate_tokens_for_value(&payload));
    write_native_tool_result(
        root,
        session,
        task_id,
        &invocation_id,
        sequence,
        "packet28.search",
        suite_packet_core::ToolOperationKind::Search,
        request_summary,
        result_summary,
        "native_tool",
        raw_est_tokens,
        reduced_est_tokens,
        Some(query.to_string()),
        None,
        search_result.paths.clone(),
        search_result.regions.clone(),
        search_result.symbols.clone(),
        artifact_id,
        None,
        duration_ms,
    )?;
    Ok(payload)
}

pub(crate) fn handle_packet28_fetch_tool_result(
    root: &Path,
    args: Packet28FetchToolResultArgs,
) -> Result<Value> {
    let task_id = args.task_id.trim();
    if task_id.is_empty() {
        return Err(anyhow!("packet28.fetch_tool_result requires task_id"));
    }
    let (artifact_id, mut payload) = load_tool_result_artifact(
        root,
        task_id,
        args.artifact_id.as_deref(),
        args.invocation_id.as_deref(),
    )?;
    if payload.get("artifact_id").is_none() {
        payload["artifact_id"] = json!(artifact_id.clone());
    }
    if payload.get("response_mode").is_none() {
        payload["response_mode"] = json!("full");
    }
    Ok(payload)
}

pub(crate) fn handle_packet28_fetch_raw_output(
    root: &Path,
    args: Packet28FetchRawOutputArgs,
) -> Result<Value> {
    let task_id = args.task_id.trim();
    if task_id.is_empty() {
        return Err(anyhow!("packet28.fetch_raw_output requires task_id"));
    }
    let (path, content) = load_raw_output_artifact(root, task_id, &args.handle)?;
    Ok(json!({
        "task_id": task_id,
        "handle": args.handle,
        "path": path,
        "content": content,
        "line_count": content.lines().count(),
    }))
}

pub(crate) fn handle_packet28_fetch_context(
    root: &Path,
    args: Packet28FetchContextArgs,
) -> Result<Value> {
    let task_id = args.task_id.trim();
    if task_id.is_empty() {
        return Err(anyhow!("packet28.fetch_context requires task_id"));
    }
    let artifact_id = args
        .artifact_id
        .or(args.context_version)
        .ok_or_else(|| anyhow!("packet28.fetch_context requires artifact_id or context_version"))?;
    let path = task_version_json_path(root, task_id, &artifact_id);
    let bytes = fs::read(&path).with_context(|| {
        format!(
            "failed to read stored broker context artifact '{}'",
            path.display()
        )
    })?;
    let mut payload: Value = serde_json::from_slice(&bytes)?;
    if payload.get("artifact_id").is_none() {
        payload["artifact_id"] = json!(artifact_id.clone());
    }
    // Honour response_mode: when slim is requested, strip heavy section
    // data and keep only the metadata the agent needs to decide next steps.
    if matches!(args.response_mode, Some(BrokerResponseMode::Slim)) {
        payload.as_object_mut().map(|obj| {
            obj.remove("sections");
            obj.remove("delta");
            obj.remove("evidence_cache");
            obj.remove("search_evidence");
            obj.remove("code_evidence");
        });
        payload["response_mode"] = json!("slim");
    } else if payload.get("response_mode").is_none() {
        payload["response_mode"] = json!("full");
    }
    Ok(payload)
}

pub(crate) fn handle_packet28_prepare_handoff(
    root: &Path,
    args: Packet28PrepareHandoffArgs,
) -> Result<Value> {
    let response = crate::broker_client::prepare_handoff(
        root,
        BrokerPrepareHandoffRequest {
            task_id: args.task_id,
            query: args.query,
            response_mode: args.response_mode,
            include_debug_memory: false,
        },
    )?;
    Ok(serde_json::to_value(response)?)
}

pub(crate) fn handle_packet28_write_intention(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    args: Packet28WriteIntentionArgs,
) -> Result<Value> {
    let text = args.text.trim();
    if text.is_empty() {
        return Err(anyhow!("packet28.write_intention requires text"));
    }
    if args.task_id.trim().is_empty() {
        return Err(anyhow!("packet28.write_intention requires task_id"));
    }
    crate::cmd_mcp::support::track_task(session, root, &args.task_id)?;
    let response = crate::broker_client::write_intention(
        root,
        BrokerWriteStateRequest {
            task_id: args.task_id,
            text: Some(text.to_string()),
            note: args.note,
            step_id: args.step_id,
            question_id: args.question_id,
            paths: args.paths,
            symbols: args.symbols,
            refresh_context: Some(false),
            ..BrokerWriteStateRequest::default()
        },
    )?;
    Ok(serde_json::to_value(response)?)
}

pub(crate) fn handle_packet28_read_regions(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    args: Packet28ReadRegionsArgs,
) -> Result<Value> {
    let task_id = args.task_id.trim();
    if task_id.is_empty() {
        return Err(anyhow!("packet28.read_regions requires task_id"));
    }
    let (sequence, invocation_id) = next_task_invocation(session, task_id)?;
    let request_summary = read_regions_request_summary(&args, &args.path);

    let started_at = Instant::now();
    let read_result = match packet28_reducer_core::read_regions(
        root,
        &packet28_reducer_core::ReadRegionsRequest {
            path: args.path.clone(),
            regions: args.regions.clone(),
            line_start: args.line_start,
            line_end: args.line_end,
        },
    ) {
        Ok(result) => result,
        Err(error) => {
            let duration_ms = started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
            write_native_tool_failure(
                root,
                session,
                task_id,
                &invocation_id,
                sequence,
                "packet28.read_regions",
                suite_packet_core::ToolOperationKind::Read,
                request_summary,
                error.to_string(),
                "native_tool",
                None,
                None,
                None,
                None,
                duration_ms,
            )?;
            return Err(error);
        }
    };
    let duration_ms = started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    let result_summary = format!(
        "Read {} line(s) from {} across {} region(s)",
        read_result.lines.len(),
        read_result.path,
        read_result.regions.len()
    );
    let full_payload = json!({
        "task_id": task_id,
        "invocation_id": invocation_id,
        "sequence": sequence,
        "path": read_result.path,
        "regions": read_result.regions,
        "symbols": read_result.symbols,
        "lines": read_result.lines,
        "compact_preview": read_result.compact_preview,
        "response_mode": "full",
    });
    let artifact_id = Some(store_result_artifact(
        root,
        task_id,
        full_payload["invocation_id"].as_str().unwrap_or_default(),
        &full_payload,
    )?);
    let payload = match args.response_mode {
        Packet28SearchResponseMode::Full => {
            let mut payload = full_payload.clone();
            payload["artifact_id"] = json!(artifact_id.clone());
            payload
        }
        Packet28SearchResponseMode::Slim => json!({
            "path": read_result.path,
            "regions": read_result.regions,
            "symbols": read_result.symbols,
            "compact_preview": read_result.compact_preview,
            "artifact_id": artifact_id.clone(),
            "response_mode": "slim",
        }),
    };
    let raw_est_tokens = Some(estimate_tokens_for_value(&full_payload));
    let reduced_est_tokens = Some(estimate_tokens_for_value(&payload));
    write_native_tool_result(
        root,
        session,
        task_id,
        payload["invocation_id"].as_str().unwrap_or_default(),
        sequence,
        "packet28.read_regions",
        suite_packet_core::ToolOperationKind::Read,
        request_summary,
        result_summary,
        "native_tool",
        raw_est_tokens,
        reduced_est_tokens,
        None,
        None,
        vec![payload["path"].as_str().unwrap_or_default().to_string()],
        json_array_strings(&full_payload, "regions"),
        json_array_strings(&full_payload, "symbols"),
        artifact_id,
        None,
        duration_ms,
    )?;
    Ok(payload)
}

fn collect_glob_matches(
    root: &Path,
    pattern: &str,
    requested_paths: &[String],
) -> Result<(Vec<String>, Vec<String>)> {
    let compiled =
        Pattern::new(pattern).with_context(|| format!("invalid glob pattern '{pattern}'"))?;
    let mut stack = Vec::<std::path::PathBuf>::new();
    let mut resolved_paths = Vec::<String>::new();
    if requested_paths.is_empty() {
        stack.push(root.to_path_buf());
    } else {
        for requested in requested_paths {
            let normalized = packet28_reducer_core::normalize_capture_path(root, requested);
            if normalized.is_empty() {
                continue;
            }
            let candidate = root.join(&normalized);
            if candidate.exists() {
                resolved_paths.push(normalized);
                stack.push(candidate);
            }
        }
    }
    let mut matches = Vec::<String>::new();
    while let Some(path) = stack.pop() {
        if path.is_dir() {
            for entry in fs::read_dir(&path)
                .with_context(|| format!("failed to read directory '{}'", path.display()))?
            {
                let entry = entry?;
                let child = entry.path();
                let relative = packet28_reducer_core::normalize_capture_path(
                    root,
                    &child.display().to_string(),
                );
                if relative.starts_with(".git/") || relative.starts_with(".packet28/") {
                    continue;
                }
                if child.is_dir() {
                    stack.push(child);
                    continue;
                }
                if !relative.is_empty() && compiled.matches(&relative) {
                    matches.push(relative);
                }
            }
        } else {
            let relative =
                packet28_reducer_core::normalize_capture_path(root, &path.display().to_string());
            if !relative.is_empty() && compiled.matches(&relative) {
                matches.push(relative);
            }
        }
    }
    matches.sort();
    matches.dedup();
    resolved_paths.sort();
    resolved_paths.dedup();
    Ok((resolved_paths, matches))
}

fn render_glob_compact_preview(pattern: &str, matches: &[String]) -> String {
    let mut rendered = vec![format!(
        "Glob matched {} path(s) for '{}'.",
        matches.len(),
        pattern
    )];
    for path in matches.iter().take(12) {
        rendered.push(path.clone());
    }
    if matches.len() > 12 {
        rendered.push(format!("+{} more path(s)", matches.len() - 12));
    }
    rendered.join("\n")
}

pub(crate) fn handle_packet28_glob(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    args: Packet28GlobArgs,
) -> Result<Value> {
    let task_id = args.task_id.trim();
    if task_id.is_empty() {
        return Err(anyhow!("packet28.glob requires task_id"));
    }
    let pattern = args.pattern.trim();
    if pattern.is_empty() {
        return Err(anyhow!("packet28.glob requires pattern"));
    }
    let (sequence, invocation_id) = next_task_invocation(session, task_id)?;
    let request_summary = glob_request_summary(&args);
    let started_at = Instant::now();
    let (resolved_paths, mut matches) = match collect_glob_matches(root, pattern, &args.paths) {
        Ok(result) => result,
        Err(error) => {
            let duration_ms = started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
            write_native_tool_failure(
                root,
                session,
                task_id,
                &invocation_id,
                sequence,
                "packet28.glob",
                suite_packet_core::ToolOperationKind::Search,
                request_summary,
                error.to_string(),
                "native_tool",
                None,
                None,
                None,
                None,
                duration_ms,
            )?;
            return Err(error);
        }
    };
    let duration_ms = started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    let max_results = args.max_results.unwrap_or(200).clamp(1, 500);
    let truncated = matches.len() > max_results;
    if truncated {
        matches.truncate(max_results);
    }
    let compact_preview = render_glob_compact_preview(pattern, &matches);
    let result_summary = compact_preview
        .lines()
        .next()
        .unwrap_or("Glob completed")
        .to_string();
    let slim_preview = result_summary.clone();
    let matched_paths = matches.clone();
    let symbols = packet28_reducer_core::infer_symbols_from_pattern(pattern);
    let full_payload = json!({
        "task_id": task_id,
        "invocation_id": invocation_id,
        "sequence": sequence,
        "pattern": pattern,
        "requested_paths": args.paths,
        "resolved_paths": resolved_paths,
        "match_count": matches.len(),
        "truncated": truncated,
        "paths": matched_paths.clone(),
        "symbols": symbols.clone(),
        "compact_preview": compact_preview,
        "response_mode": "full",
    });
    let artifact_id = Some(store_result_artifact(
        root,
        task_id,
        full_payload["invocation_id"].as_str().unwrap_or_default(),
        &full_payload,
    )?);
    let payload = match args.response_mode {
        Packet28SearchResponseMode::Full => {
            let mut payload = full_payload.clone();
            payload["artifact_id"] = json!(artifact_id.clone());
            payload
        }
        Packet28SearchResponseMode::Slim => json!({
            "match_count": matches.len(),
            "compact_preview": slim_preview,
            "artifact_id": artifact_id.clone(),
            "response_mode": "slim",
        }),
    };
    let raw_est_tokens = Some(estimate_tokens_for_value(&full_payload));
    let reduced_est_tokens = Some(estimate_tokens_for_value(&payload));
    write_native_tool_result(
        root,
        session,
        task_id,
        &invocation_id,
        sequence,
        "packet28.glob",
        suite_packet_core::ToolOperationKind::Search,
        request_summary,
        result_summary,
        "native_tool",
        raw_est_tokens,
        reduced_est_tokens,
        Some(pattern.to_string()),
        None,
        matched_paths,
        Vec::new(),
        symbols,
        artifact_id,
        None,
        duration_ms,
    )?;
    Ok(payload)
}
