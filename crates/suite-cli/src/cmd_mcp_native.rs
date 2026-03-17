use super::*;
use crate::cmd_mcp::support::{
    load_tool_result_artifact, next_task_invocation, store_result_artifact,
    write_auto_capture_state_batch_via_session,
};

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
    search_query: Option<String>,
    command: Option<String>,
    paths: Vec<String>,
    regions: Vec<String>,
    symbols: Vec<String>,
    artifact_id: Option<String>,
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
            search_query,
            command,
            sequence: Some(sequence),
            duration_ms: Some(duration_ms),
            paths,
            regions,
            symbols,
            artifact_id,
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
    command: Option<String>,
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
            error_class: Some(classify_error_message(&error_message)),
            error_message: Some(error_message.clone()),
            retryable: Some(is_retryable_error(&error_message)),
            sequence: Some(sequence),
            duration_ms: Some(duration_ms),
            command,
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
            let mut payload = full_payload;
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
        Some(query.to_string()),
        None,
        search_result.paths.clone(),
        search_result.regions.clone(),
        search_result.symbols.clone(),
        artifact_id,
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
        },
    )?;
    Ok(serde_json::to_value(response)?)
}

pub(crate) fn handle_packet28_write_intention(
    root: &Path,
    args: Packet28WriteIntentionArgs,
) -> Result<Value> {
    let text = args.text.trim();
    if text.is_empty() {
        return Err(anyhow!("packet28.write_intention requires text"));
    }
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
    let payload = json!({
        "task_id": task_id,
        "invocation_id": invocation_id,
        "sequence": sequence,
        "path": read_result.path,
        "regions": read_result.regions,
        "symbols": read_result.symbols,
        "lines": read_result.lines,
        "compact_preview": read_result.compact_preview,
    });
    let artifact_id = maybe_store_result_artifact(
        root,
        task_id,
        payload["invocation_id"].as_str().unwrap_or_default(),
        &payload,
        false,
    )?;
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
        None,
        None,
        vec![payload["path"].as_str().unwrap_or_default().to_string()],
        json_array_strings(&payload, "regions"),
        json_array_strings(&payload, "symbols"),
        artifact_id,
        duration_ms,
    )?;
    Ok(payload)
}
