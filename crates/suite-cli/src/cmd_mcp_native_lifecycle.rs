use super::*;

pub(crate) fn tool_descriptors() -> Vec<Value> {
    let mut tools = search_descriptors();
    tools.extend(read_descriptors());
    tools.extend(artifact_descriptors());
    tools.extend(handoff_descriptors());
    tools.extend(agent_descriptors());
    tools
}

fn search_descriptors() -> Vec<Value> {
    vec![
        json!({
            "name": "packet28.search",
            "description": "Run compact code/text search and return a slim preview plus a fetchable full artifact.",
            "inputSchema": {
                "type": "object",
                "required": ["query"],
                "properties": {
                    "task_id": {"type":"string"},
                    "query": {"type":"string"},
                    "paths": {"type":"array","items":{"type":"string"}},
                    "fixed_string": {"type":"boolean"},
                    "case_sensitive": {"type":"boolean"},
                    "whole_word": {"type":"boolean"},
                    "context_lines": {"type":"integer","minimum":0},
                    "max_matches_per_file": {"type":"integer","minimum":1},
                    "max_total_matches": {"type":"integer","minimum":1},
                    "search_strategy": {"type":"string","enum":["hybrid","recall","indexed","native","fff"]},
                    "response_mode": {"type":"string","enum":["slim","full"]}
                }
            }
        }),
        json!({
            "name": "packet28.search_fast",
            "description": "Run compact code/text search over the persistent daemon socket without storing artifacts or broker state.",
            "inputSchema": {
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": {"type":"string"},
                    "paths": {"type":"array","items":{"type":"string"}},
                    "fixed_string": {"type":"boolean"},
                    "case_sensitive": {"type":"boolean"},
                    "whole_word": {"type":"boolean"},
                    "context_lines": {"type":"integer","minimum":0},
                    "max_matches_per_file": {"type":"integer","minimum":1},
                    "max_total_matches": {"type":"integer","minimum":1},
                    "search_strategy": {"type":"string","enum":["hybrid","recall","indexed","native","fff"]},
                    "response_mode": {"type":"string","enum":["slim","full"]}
                }
            }
        }),
    ]
}

fn read_descriptors() -> Vec<Value> {
    vec![
        json!({
            "name": "packet28.read_regions",
            "description": "Read targeted file regions and return a slim preview plus a fetchable full artifact.",
            "inputSchema": {
                "type": "object",
                "required": ["path"],
                "properties": {
                    "task_id": {"type":"string"},
                    "path": {"type":"string"},
                    "regions": {"type":"array","items":{"type":"string"}},
                    "line_start": {"type":"integer","minimum":1},
                    "line_end": {"type":"integer","minimum":1},
                    "response_mode": {"type":"string","enum":["slim","full"]}
                }
            }
        }),
        json!({
            "name": "packet28.glob",
            "description": "Resolve a glob pattern into compact path matches with a fetchable full artifact.",
            "inputSchema": {
                "type": "object",
                "required": ["pattern"],
                "properties": {
                    "task_id": {"type":"string"},
                    "pattern": {"type":"string"},
                    "paths": {"type":"array","items":{"type":"string"}},
                    "max_results": {"type":"integer","minimum":1},
                    "response_mode": {"type":"string","enum":["slim","full"]}
                }
            }
        }),
    ]
}

fn artifact_descriptors() -> Vec<Value> {
    vec![
        json!({
            "name": "packet28.fetch_tool_result",
            "description": "Fetch a previously stored full artifact for packet28.search, packet28.read_regions, packet28.glob, or hook-captured tool output.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task_id": {"type":"string"},
                    "artifact_id": {"type":"string"},
                    "invocation_id": {"type":"string"}
                }
            }
        }),
        json!({
            "name": "packet28.fetch_raw_output",
            "description": "Fetch raw output from a hook spool file or other Packet28 raw artifact handle.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task_id": {"type":"string"},
                    "handle": {"type":"string"}
                }
            }
        }),
        json!({
            "name": "packet28.fetch_context",
            "description": "Fetch a stored Packet28 broker context by context_version or artifact_id. Use response_mode='slim' to omit heavy sections.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task_id": {"type":"string"},
                    "artifact_id": {"type":"string"},
                    "context_version": {"type":"string"},
                    "response_mode": {"type":"string","enum":["slim","full"]}
                }
            }
        }),
    ]
}

fn handoff_descriptors() -> Vec<Value> {
    vec![
        json!({
            "name": "packet28.verify_handoff",
            "description": "Verify a stored Packet28 handoff context artifact has enough objective, next-action, debt, and evidence signal for replay.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task_id": {"type":"string"},
                    "artifact_id": {"type":"string"},
                    "context_version": {"type":"string"}
                }
            }
        }),
        json!({
            "name": "packet28.prompt_pressure",
            "description": "Estimate whether a stored handoff context plus the next worker prompt will fit a target token budget and identify the largest removable sections.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task_id": {"type":"string"},
                    "artifact_id": {"type":"string"},
                    "context_version": {"type":"string"},
                    "next_prompt": {"type":"string"},
                    "budget_tokens": {"type":"integer","minimum":1}
                }
            }
        }),
        json!({
            "name": "packet28.handoff_diff",
            "description": "Compare two stored Packet28 handoff context artifacts and report compact objective, next-action, evidence, and debt-signal deltas.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task_id": {"type":"string"},
                    "left_artifact_id": {"type":"string"},
                    "left_context_version": {"type":"string"},
                    "right_artifact_id": {"type":"string"},
                    "right_context_version": {"type":"string"}
                }
            }
        }),
        json!({
            "name": "packet28.handoff_compress",
            "description": "Recommend non-critical handoff sections to remove so an over-budget replay packet can fit while preserving objective, next-action, debt, and evidence signals.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task_id": {"type":"string"},
                    "artifact_id": {"type":"string"},
                    "context_version": {"type":"string"},
                    "next_prompt": {"type":"string"},
                    "budget_tokens": {"type":"integer","minimum":1}
                }
            }
        }),
        handoff_artifact_descriptor(
            "packet28.handoff_lint_dependencies",
            "Lint a stored Packet28 handoff artifact for referenced artifact handles that are missing from its available evidence handles.",
        ),
        handoff_artifact_descriptor(
            "packet28.handoff_lint_paths",
            "Lint a stored Packet28 handoff artifact for repo-relative path references that are absent on disk and not listed as changed paths.",
        ),
        handoff_artifact_descriptor(
            "packet28.handoff_lint_tests",
            "Lint a stored Packet28 handoff artifact for test-like names that are mentioned without a runnable test command.",
        ),
        handoff_artifact_descriptor(
            "packet28.handoff_lint_stale_commands",
            "Lint a stored Packet28 handoff artifact for referenced commands that ran before the latest relevant edit event.",
        ),
        handoff_artifact_descriptor(
            "packet28.handoff_lint_environment",
            "Lint a stored Packet28 handoff artifact for command references that depend on missing environment variables or executables.",
        ),
        handoff_artifact_descriptor(
            "packet28.handoff_lint_all",
            "Run replay readiness plus all Packet28 handoff linters and return one bounded readiness decision.",
        ),
        handoff_artifact_descriptor(
            "packet28.handoff_fix_plan",
            "Plan concrete repair actions for failing Packet28 handoff lint categories.",
        ),
        json!({
            "name": "packet28.handoff_repair_verify",
            "description": "Compare handoff lint categories before and after a repair and report cleared, remaining, and regressed categories.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task_id": {"type":"string"},
                    "before_artifact_id": {"type":"string"},
                    "before_context_version": {"type":"string"},
                    "after_artifact_id": {"type":"string"},
                    "after_context_version": {"type":"string"}
                }
            }
        }),
        handoff_history_descriptor(
            "packet28.handoff_lint_trends",
            "Report recurring, cleared, and latest blocking handoff lint categories across stored task artifacts.",
        ),
        handoff_history_descriptor(
            "packet28.handoff_lint_regressions",
            "Detect handoff lint categories that were cleared and then reappeared in the latest artifact.",
        ),
        json!({
            "name": "packet28.prepare_handoff",
            "description": "Prepare a compact Packet28 handoff packet for bootstrapping a fresh worker after a checkpoint.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task_id": {"type":"string"},
                    "query": {"type":"string"},
                    "response_mode": {"type":"string","enum":["slim","full"]}
                }
            }
        }),
    ]
}

fn handoff_artifact_descriptor(name: &str, description: &str) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": {
                "task_id": {"type":"string"},
                "artifact_id": {"type":"string"},
                "context_version": {"type":"string"}
            }
        }
    })
}

fn handoff_history_descriptor(name: &str, description: &str) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": {
                "task_id": {"type":"string"},
                "artifact_ids": {"type":"array","items":{"type":"string"}},
                "max_artifacts": {"type":"integer","minimum":1}
            }
        }
    })
}

fn agent_descriptors() -> Vec<Value> {
    vec![
        json!({
            "name": "packet28.validate_plan",
            "description": "Validate an agent implementation plan against Packet28 broker state, coverage, dependency order, read-before-edit, and mapped test-gate evidence.",
            "inputSchema": {
                "type": "object",
                "required": ["task_id", "steps"],
                "properties": {
                    "task_id": {"type":"string"},
                    "steps": {
                        "type":"array",
                        "items": {
                            "type":"object",
                            "properties": {
                                "id": {"type":"string"},
                                "action": {"type":"string"},
                                "description": {"type":"string"},
                                "paths": {"type":"array","items":{"type":"string"}},
                                "symbols": {"type":"array","items":{"type":"string"}},
                                "depends_on": {"type":"array","items":{"type":"string"}}
                            }
                        }
                    },
                    "require_read_before_edit": {"type":"boolean"},
                    "require_test_gate": {"type":"boolean"},
                    "budget_tokens": {"type":"integer","minimum":1}
                }
            }
        }),
        json!({
            "name": "packet28.action_critic",
            "description": "Return focused Packet28 action-critic warnings before choosing a tool or editing focused files.",
            "inputSchema": {
                "type": "object",
                "required": ["task_id", "action"],
                "properties": {
                    "task_id": {"type":"string"},
                    "action": {"type":"string","enum":["choose_tool","edit"]},
                    "query": {"type":"string"},
                    "tool_name": {"type":"string"},
                    "focus_paths": {"type":"array","items":{"type":"string"}},
                    "focus_symbols": {"type":"array","items":{"type":"string"}},
                    "budget_tokens": {"type":"integer","minimum":1}
                }
            }
        }),
        json!({
            "name": "packet28.recommend_next_tool",
            "description": "Recommend one or two next Packet28 commands using local ROI, failure-advice, and focused freshness signals.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task_id": {"type":"string"},
                    "query": {"type":"string"},
                    "focus_paths": {"type":"array","items":{"type":"string"}},
                    "focus_symbols": {"type":"array","items":{"type":"string"}},
                    "max_recommendations": {"type":"integer","minimum":1,"maximum":4}
                }
            }
        }),
        json!({
            "name": "packet28.validate_tool_outcome",
            "description": "Classify the latest matching Packet28 tool outcome as success, fallback, missing artifact, stale artifact, or failure.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task_id": {"type":"string"},
                    "command": {"type":"string"},
                    "focus_paths": {"type":"array","items":{"type":"string"}}
                }
            }
        }),
        json!({
            "name": "packet28.agent_status",
            "description": "Return local Packet28 agent integration health, active task state, hook config presence, and reducer cache safety.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task_id": {"type":"string"}
                }
            }
        }),
        json!({
            "name": "packet28.patch_risk",
            "description": "Score pre-edit patch risk from path scope, cached testmap mappings, and recent failure/fallback records.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task_id": {"type":"string"},
                    "paths": {"type":"array","items":{"type":"string"}}
                }
            }
        }),
    ]
}

pub(crate) fn handle_tool_call(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    name: &str,
    arguments: &Value,
) -> Result<Option<Value>> {
    let Some(payload) = dispatch_payload(root, session, name, arguments)? else {
        return Ok(None);
    };
    let summary = summarize_payload(name, &payload);
    Ok(Some(crate::cmd_mcp::response::shape_tool_response(
        payload, summary,
    )))
}

fn dispatch_payload(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    name: &str,
    arguments: &Value,
) -> Result<Option<Value>> {
    let payload = match name {
        "packet28.search" => {
            let mut request: Packet28SearchArgs = serde_json::from_value(arguments.clone())?;
            request.task_id = resolve_session_task_id(
                session,
                root,
                &request.task_id,
                Some(request.query.as_str()),
                "packet28.search",
            )?;
            track_task(session, root, &request.task_id)?;
            handle_packet28_search(root, session, request)?
        }
        "packet28.search_fast" => {
            let request: Packet28SearchFastArgs = serde_json::from_value(arguments.clone())?;
            handle_packet28_search_fast(root, session, request)?
        }
        "packet28.read_regions" => {
            let mut request: Packet28ReadRegionsArgs = serde_json::from_value(arguments.clone())?;
            request.task_id = resolve_session_task_id(
                session,
                root,
                &request.task_id,
                Some(request.path.as_str()),
                "packet28.read_regions",
            )?;
            track_task(session, root, &request.task_id)?;
            handle_packet28_read_regions(root, session, request)?
        }
        "packet28.glob" => {
            let mut request: Packet28GlobArgs = serde_json::from_value(arguments.clone())?;
            request.task_id = resolve_session_task_id(
                session,
                root,
                &request.task_id,
                Some(request.pattern.as_str()),
                "packet28.glob",
            )?;
            track_task(session, root, &request.task_id)?;
            handle_packet28_glob(root, session, request)?
        }
        "packet28.fetch_tool_result" => {
            let mut request: Packet28FetchToolResultArgs =
                serde_json::from_value(arguments.clone())?;
            request.task_id = crate::cmd_mcp::support::resolve_artifact_task_id(
                session,
                root,
                &request.task_id,
                None,
                "packet28.fetch_tool_result",
            )?;
            handle_packet28_fetch_tool_result(root, request)?
        }
        "packet28.fetch_raw_output" => {
            let mut request: Packet28FetchRawOutputArgs =
                serde_json::from_value(arguments.clone())?;
            request.task_id = crate::cmd_mcp::support::resolve_artifact_task_id(
                session,
                root,
                &request.task_id,
                Some(request.handle.as_str()),
                "packet28.fetch_raw_output",
            )?;
            handle_packet28_fetch_raw_output(root, request)?
        }
        "packet28.fetch_context" => {
            let mut request: Packet28FetchContextArgs = serde_json::from_value(arguments.clone())?;
            request.task_id = crate::cmd_mcp::support::resolve_artifact_task_id(
                session,
                root,
                &request.task_id,
                None,
                "packet28.fetch_context",
            )?;
            handle_packet28_fetch_context(root, request)?
        }
        "packet28.verify_handoff" => {
            let mut request: Packet28VerifyHandoffArgs = serde_json::from_value(arguments.clone())?;
            request.task_id = crate::cmd_mcp::support::resolve_artifact_task_id(
                session,
                root,
                &request.task_id,
                request
                    .artifact_id
                    .as_deref()
                    .or(request.context_version.as_deref()),
                name,
            )?;
            native_tools::handle_packet28_verify_handoff(root, request)?
        }
        "packet28.prompt_pressure" => {
            let mut request: Packet28PromptPressureArgs =
                serde_json::from_value(arguments.clone())?;
            request.task_id = resolve_session_task_id(
                session,
                root,
                &request.task_id,
                request
                    .artifact_id
                    .as_deref()
                    .or(request.context_version.as_deref()),
                name,
            )?;
            track_task(session, root, &request.task_id)?;
            native_tools::handle_packet28_prompt_pressure(root, request)?
        }
        "packet28.handoff_diff" => {
            let mut request: Packet28HandoffDiffArgs = serde_json::from_value(arguments.clone())?;
            request.task_id = resolve_session_task_id(
                session,
                root,
                &request.task_id,
                request
                    .left_artifact_id
                    .as_deref()
                    .or(request.left_context_version.as_deref())
                    .or(request.right_artifact_id.as_deref())
                    .or(request.right_context_version.as_deref()),
                name,
            )?;
            track_task(session, root, &request.task_id)?;
            native_tools::handle_packet28_handoff_diff(root, request)?
        }
        "packet28.handoff_compress" => {
            let mut request: Packet28HandoffCompressionArgs =
                serde_json::from_value(arguments.clone())?;
            request.task_id = resolve_session_task_id(
                session,
                root,
                &request.task_id,
                request
                    .artifact_id
                    .as_deref()
                    .or(request.context_version.as_deref()),
                name,
            )?;
            track_task(session, root, &request.task_id)?;
            native_tools::handle_packet28_handoff_compress(root, request)?
        }
        "packet28.handoff_lint_dependencies" => {
            let mut request: Packet28HandoffDependencyLintArgs =
                serde_json::from_value(arguments.clone())?;
            request.task_id = resolve_session_task_id(
                session,
                root,
                &request.task_id,
                request
                    .artifact_id
                    .as_deref()
                    .or(request.context_version.as_deref()),
                name,
            )?;
            track_task(session, root, &request.task_id)?;
            native_tools::handle_packet28_handoff_lint_dependencies(root, request)?
        }
        "packet28.handoff_lint_paths" => {
            let mut request: Packet28HandoffPathLintArgs =
                serde_json::from_value(arguments.clone())?;
            request.task_id = resolve_session_task_id(
                session,
                root,
                &request.task_id,
                request
                    .artifact_id
                    .as_deref()
                    .or(request.context_version.as_deref()),
                name,
            )?;
            track_task(session, root, &request.task_id)?;
            native_tools::handle_packet28_handoff_lint_paths(root, request)?
        }
        "packet28.handoff_lint_tests" => {
            let mut request: Packet28HandoffTestLintArgs =
                serde_json::from_value(arguments.clone())?;
            request.task_id = resolve_session_task_id(
                session,
                root,
                &request.task_id,
                request
                    .artifact_id
                    .as_deref()
                    .or(request.context_version.as_deref()),
                name,
            )?;
            track_task(session, root, &request.task_id)?;
            native_tools::handle_packet28_handoff_lint_tests(root, request)?
        }
        "packet28.handoff_lint_stale_commands" => {
            let mut request: Packet28HandoffStaleCommandLintArgs =
                serde_json::from_value(arguments.clone())?;
            request.task_id = resolve_session_task_id(
                session,
                root,
                &request.task_id,
                request
                    .artifact_id
                    .as_deref()
                    .or(request.context_version.as_deref()),
                name,
            )?;
            track_task(session, root, &request.task_id)?;
            native_tools::handle_packet28_handoff_lint_stale_commands(root, request)?
        }
        "packet28.handoff_lint_environment" => {
            let mut request: Packet28HandoffEnvironmentLintArgs =
                serde_json::from_value(arguments.clone())?;
            request.task_id = resolve_session_task_id(
                session,
                root,
                &request.task_id,
                request
                    .artifact_id
                    .as_deref()
                    .or(request.context_version.as_deref()),
                name,
            )?;
            track_task(session, root, &request.task_id)?;
            native_tools::handle_packet28_handoff_lint_environment(root, request)?
        }
        "packet28.handoff_lint_all" => {
            let mut request: Packet28HandoffLintAllArgs =
                serde_json::from_value(arguments.clone())?;
            request.task_id = resolve_session_task_id(
                session,
                root,
                &request.task_id,
                request
                    .artifact_id
                    .as_deref()
                    .or(request.context_version.as_deref()),
                name,
            )?;
            track_task(session, root, &request.task_id)?;
            native_tools::handle_packet28_handoff_lint_all(root, request)?
        }
        "packet28.handoff_fix_plan" => {
            let mut request: Packet28HandoffFixPlanArgs =
                serde_json::from_value(arguments.clone())?;
            request.task_id = resolve_session_task_id(
                session,
                root,
                &request.task_id,
                request
                    .artifact_id
                    .as_deref()
                    .or(request.context_version.as_deref()),
                name,
            )?;
            track_task(session, root, &request.task_id)?;
            native_tools::handle_packet28_handoff_fix_plan(root, request)?
        }
        "packet28.handoff_repair_verify" => {
            let mut request: Packet28HandoffRepairVerifyArgs =
                serde_json::from_value(arguments.clone())?;
            request.task_id = resolve_session_task_id(
                session,
                root,
                &request.task_id,
                request
                    .after_artifact_id
                    .as_deref()
                    .or(request.after_context_version.as_deref())
                    .or(request.before_artifact_id.as_deref())
                    .or(request.before_context_version.as_deref()),
                name,
            )?;
            track_task(session, root, &request.task_id)?;
            native_tools::handle_packet28_handoff_repair_verify(root, request)?
        }
        "packet28.handoff_lint_trends" => {
            let mut request: Packet28HandoffLintTrendArgs =
                serde_json::from_value(arguments.clone())?;
            request.task_id = resolve_session_task_id(session, root, &request.task_id, None, name)?;
            track_task(session, root, &request.task_id)?;
            native_tools::handle_packet28_handoff_lint_trends(root, request)?
        }
        "packet28.handoff_lint_regressions" => {
            let mut request: Packet28HandoffLintRegressionArgs =
                serde_json::from_value(arguments.clone())?;
            request.task_id = resolve_session_task_id(session, root, &request.task_id, None, name)?;
            track_task(session, root, &request.task_id)?;
            native_tools::handle_packet28_handoff_lint_regressions(root, request)?
        }
        "packet28.prepare_handoff" | "packet28.handoff" => {
            let mut request: Packet28PrepareHandoffArgs =
                serde_json::from_value(arguments.clone())?;
            request.task_id = resolve_session_task_id(session, root, &request.task_id, None, name)?;
            track_task(session, root, &request.task_id)?;
            handle_packet28_prepare_handoff(root, request)?
        }
        "packet28.validate_plan" => {
            let mut request: Packet28ValidatePlanArgs = serde_json::from_value(arguments.clone())?;
            request.task_id = resolve_session_task_id(session, root, &request.task_id, None, name)?;
            track_task(session, root, &request.task_id)?;
            handle_packet28_validate_plan(root, request)?
        }
        "packet28.action_critic" => {
            let mut request: Packet28ActionCriticArgs = serde_json::from_value(arguments.clone())?;
            request.task_id = resolve_session_task_id(
                session,
                root,
                &request.task_id,
                request.query.as_deref().or(request.tool_name.as_deref()),
                name,
            )?;
            track_task(session, root, &request.task_id)?;
            native_tools::handle_packet28_action_critic(root, request)?
        }
        "packet28.recommend_next_tool" => {
            let mut request: Packet28RecommendNextToolArgs =
                serde_json::from_value(arguments.clone())?;
            request.task_id = resolve_session_task_id(
                session,
                root,
                &request.task_id,
                request.query.as_deref(),
                name,
            )?;
            track_task(session, root, &request.task_id)?;
            native_tools::handle_packet28_recommend_next_tool(root, request)?
        }
        "packet28.validate_tool_outcome" => {
            let mut request: Packet28ValidateToolOutcomeArgs =
                serde_json::from_value(arguments.clone())?;
            request.task_id = resolve_session_task_id(
                session,
                root,
                &request.task_id,
                request.command.as_deref(),
                name,
            )?;
            track_task(session, root, &request.task_id)?;
            native_tools::handle_packet28_validate_tool_outcome(root, request)?
        }
        "packet28.agent_status" => handle_packet28_agent_status(root, arguments.clone())?,
        "packet28.patch_risk" => {
            let mut request: Packet28PatchRiskArgs = serde_json::from_value(arguments.clone())?;
            request.task_id = resolve_session_task_id(
                session,
                root,
                &request.task_id,
                request.paths.first().map(String::as_str),
                name,
            )?;
            track_task(session, root, &request.task_id)?;
            native_tools::handle_packet28_patch_risk(root, request)?
        }
        _ => return Ok(None),
    };
    Ok(Some(payload))
}

fn summarize_payload(name: &str, payload: &Value) -> String {
    match name {
        "packet28.search" | "packet28.search_fast" | "packet28.read_regions" | "packet28.glob" => {
            payload
                .get("compact_preview")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| "Packet28 compact tool result.".to_string())
        }
        "packet28.fetch_tool_result" => {
            let artifact_id = payload
                .get("artifact_id")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            format!("Packet28 fetched tool artifact {artifact_id}.")
        }
        "packet28.fetch_raw_output" => {
            let path = payload
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            format!("Packet28 fetched raw output from {path}.")
        }
        "packet28.fetch_context" => {
            let artifact_id = payload
                .get("artifact_id")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            format!("Packet28 fetched broker context artifact {artifact_id}.")
        }
        "packet28.verify_handoff" => {
            let ready = payload
                .get("ready")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let score = payload
                .get("score")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            format!("Packet28 handoff replay ready={ready} score={score}.")
        }
        "packet28.prompt_pressure" => {
            let pressure = payload
                .get("pressure")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let remaining = payload
                .get("remaining_tokens")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            format!("Packet28 prompt pressure={pressure} remaining_tokens={remaining}.")
        }
        "packet28.handoff_diff" => {
            let delta_count = payload
                .get("delta_count")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            let top_delta = payload
                .get("top_delta")
                .and_then(Value::as_str)
                .unwrap_or("none");
            format!("Packet28 handoff diff delta_count={delta_count} top_delta={top_delta}.")
        }
        "packet28.handoff_compress" => {
            let recommendation_count = payload
                .get("recommendations")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or_default();
            let projected_over_budget = payload
                .get("projected_over_budget")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            format!(
                "Packet28 handoff compression recommendations={recommendation_count} \
                 projected_over_budget={projected_over_budget}."
            )
        }
        "packet28.handoff_lint_dependencies" => lint_summary("dependency", payload),
        "packet28.handoff_lint_paths" => lint_summary("path", payload),
        "packet28.handoff_lint_tests" => lint_summary("test", payload),
        "packet28.handoff_lint_stale_commands" => lint_summary("stale-command", payload),
        "packet28.handoff_lint_environment" => lint_summary("environment", payload),
        "packet28.handoff_lint_all" => {
            let status = payload
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let issue_count = payload
                .get("issue_count")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            format!("Packet28 handoff lint aggregate status={status} issue_count={issue_count}.")
        }
        "packet28.handoff_fix_plan" => {
            let action_count = payload
                .get("action_count")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            format!("Packet28 handoff fix plan action_count={action_count}.")
        }
        "packet28.handoff_repair_verify" => {
            let verified = payload
                .get("verified")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            format!("Packet28 handoff repair verified={verified}.")
        }
        "packet28.handoff_lint_trends" => {
            let artifact_count = payload
                .get("artifact_count")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            format!("Packet28 handoff lint trends artifacts={artifact_count}.")
        }
        "packet28.handoff_lint_regressions" => {
            let regression_count = payload
                .get("regression_count")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            format!("Packet28 handoff lint regressions count={regression_count}.")
        }
        "packet28.prepare_handoff" | "packet28.handoff" => {
            let ready = payload
                .get("handoff_ready")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let reason = payload
                .get("handoff_reason")
                .and_then(Value::as_str)
                .unwrap_or("handoff prepared");
            if ready {
                format!("Packet28 prepared a handoff: {reason}")
            } else {
                format!("Packet28 did not prepare a handoff: {reason}")
            }
        }
        "packet28.validate_plan" => {
            let valid = payload
                .get("valid")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let violations = payload
                .get("violations")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or_default();
            let warnings = payload
                .get("warnings")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or_default();
            format!(
                "Packet28 plan validation valid={valid} violations={violations} warnings={warnings}."
            )
        }
        "packet28.action_critic" => {
            let warning_count = payload
                .get("warnings")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or_default();
            format!("Packet28 action critic returned {warning_count} warning(s).")
        }
        "packet28.recommend_next_tool" => {
            let recommendation_count = payload
                .get("recommendations")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or_default();
            let token_estimate = payload
                .get("token_estimate")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            format!(
                "Packet28 recommended {recommendation_count} next tool(s), estimated \
                 {token_estimate} tokens."
            )
        }
        "packet28.validate_tool_outcome" => {
            let status = payload
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let valid = payload
                .get("valid_success")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            format!("Packet28 tool outcome status={status} valid_success={valid}.")
        }
        "packet28.agent_status" => {
            let policy = payload
                .get("reducer_cache_safety")
                .and_then(|value| value.get("policy"))
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            format!("Packet28 agent status cache_policy={policy}.")
        }
        "packet28.patch_risk" => {
            let risk = payload
                .get("risk")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let score = payload
                .get("score")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            format!("Packet28 patch risk={risk} score={score}.")
        }
        _ => "Packet28 response.".to_string(),
    }
}

fn lint_summary(category: &str, payload: &Value) -> String {
    let issue_count = payload
        .get("issue_count")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    format!("Packet28 handoff {category} lint issue_count={issue_count}.")
}

#[cfg(test)]
pub(crate) fn structural_snapshot() -> Value {
    let descriptors = tool_descriptors();
    let representative_names = [
        "packet28.search",
        "packet28.read_regions",
        "packet28.fetch_context",
        "packet28.prepare_handoff",
        "packet28.validate_plan",
    ];
    let representative_descriptors = representative_names
        .iter()
        .filter_map(|name| {
            descriptors
                .iter()
                .find(|descriptor| descriptor["name"] == **name)
                .cloned()
        })
        .collect::<Vec<_>>();
    let response_shapes = [
        (
            "packet28.search",
            json!({"compact_preview":"Search found 2 matches.","paths":["src/lib.rs"]}),
        ),
        (
            "packet28.read_regions",
            json!({"compact_preview":"src/lib.rs:10-12","path":"src/lib.rs"}),
        ),
        (
            "packet28.fetch_context",
            json!({"artifact_id":"context-v7","response_mode":"slim"}),
        ),
        (
            "packet28.prepare_handoff",
            json!({"handoff_ready":true,"handoff_reason":"checkpoint requested"}),
        ),
        (
            "packet28.validate_plan",
            json!({"valid":false,"violations":[{"code":"missing_test"}],"warnings":[]}),
        ),
    ]
    .into_iter()
    .map(|(name, payload)| {
        let summary = summarize_payload(name, &payload);
        json!({
            "name": name,
            "response": crate::cmd_mcp::response::shape_tool_response(payload, summary),
        })
    })
    .collect::<Vec<_>>();
    let names = descriptors
        .iter()
        .filter_map(|descriptor| descriptor.get("name").and_then(Value::as_str))
        .collect::<Vec<_>>();

    json!({
        "native_tool_names": names,
        "representative_descriptors": representative_descriptors,
        "representative_response_shapes": response_shapes,
    })
}
