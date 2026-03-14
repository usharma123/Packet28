use super::*;

pub(crate) fn run_guardy_check(
    ctx: &mut ExecutionContext,
    input_packets: &[KernelPacket],
) -> Result<ReducerResult, KernelError> {
    if input_packets.len() != 1 {
        return Err(KernelError::InvalidRequest {
            detail: "guardy.check requires exactly one input packet".to_string(),
        });
    }

    let config_path = ctx
        .policy_context
        .get("config_path")
        .and_then(Value::as_str)
        .filter(|path| !path.trim().is_empty())
        .ok_or_else(|| KernelError::InvalidRequest {
            detail: "guardy.check requires policy_context.config_path".to_string(),
        })?
        .to_string();

    let config = guardy_core::ContextConfig::load(Path::new(&config_path)).map_err(|source| {
        KernelError::ReducerFailed {
            target: ctx.target.clone(),
            detail: source.to_string(),
        }
    })?;

    let packet = kernel_packet_to_guard_packet(&input_packets[0])?;

    let audit = guardy_core::check_packet(&config, &packet);

    ctx.set_shared("passed", Value::Bool(audit.passed));
    ctx.set_shared("policy_version", Value::from(audit.policy_version));

    let mut files = Vec::new();
    let mut symbols = Vec::new();
    for finding in &audit.findings {
        if finding.subject.contains('/') || finding.subject.contains('.') {
            files.push(suite_packet_core::FileRef {
                path: finding.subject.clone(),
                relevance: Some(1.0),
                source: Some("guardy.check".to_string()),
            });
        } else {
            symbols.push(suite_packet_core::SymbolRef {
                name: finding.subject.clone(),
                file: None,
                kind: Some("policy_subject".to_string()),
                relevance: Some(1.0),
                source: Some("guardy.check".to_string()),
            });
        }
    }

    let payload_bytes = serde_json::to_vec(&audit)
        .map(|buf| buf.len())
        .unwrap_or_default();
    let envelope = suite_packet_core::EnvelopeV1 {
        version: "1".to_string(),
        tool: "guardy".to_string(),
        kind: "guard_check".to_string(),
        hash: String::new(),
        summary: format!(
            "guard check passed={} findings={}",
            audit.passed,
            audit.findings.len()
        ),
        files,
        symbols,
        risk: None,
        confidence: Some(if audit.passed { 1.0 } else { 0.75 }),
        budget_cost: suite_packet_core::BudgetCost {
            est_tokens: 0,
            est_bytes: 0,
            runtime_ms: 0,
            tool_calls: 1,
            payload_est_tokens: Some((payload_bytes / 4) as u64),
            payload_est_bytes: Some(payload_bytes),
        },
        provenance: suite_packet_core::Provenance {
            inputs: vec![config_path.to_string()],
            git_base: None,
            git_head: None,
            generated_at_unix: now_unix(),
        },
        payload: audit.clone(),
    }
    .with_canonical_hash_and_real_budget();

    let packet = KernelPacket {
        packet_id: Some(format!(
            "guardy-{}",
            envelope.hash.chars().take(12).collect::<String>()
        )),
        format: default_packet_format(),
        body: serde_json::to_value(&envelope).map_err(|source| KernelError::ReducerFailed {
            target: ctx.target.clone(),
            detail: source.to_string(),
        })?,
        token_usage: Some(envelope.budget_cost.est_tokens),
        runtime_ms: Some(envelope.budget_cost.runtime_ms),
        metadata: json!({
            "reducer": "guardy.check",
            "passed": audit.passed,
            "findings": audit.findings.len(),
            "kind": "guard_check",
            "hash": envelope.hash,
        }),
    };

    Ok(ReducerResult {
        output_packets: vec![packet],
        metadata: json!({
            "reducer": "guardy.check",
            "passed": audit.passed,
        }),
    })
}

pub(crate) fn run_diffy_analyze_reducer(
    ctx: &mut ExecutionContext,
    _input_packets: &[KernelPacket],
) -> Result<ReducerResult, KernelError> {
    let input: DiffAnalyzeKernelInput =
        serde_json::from_value(ctx.reducer_input.clone()).map_err(|source| {
            KernelError::ReducerFailed {
                target: ctx.target.clone(),
                detail: format!("invalid reducer input: {source}"),
            }
        })?;

    let git_base = input.base.clone();
    let git_head = input.head.clone();
    let request = build_diff_pipeline_request(&input);
    let adapters = default_diff_pipeline_ingest_adapters();
    let output = diffy_core::pipeline::run_analysis(request, &adapters).map_err(|source| {
        KernelError::ReducerFailed {
            target: ctx.target.clone(),
            detail: source.to_string(),
        }
    })?;
    let envelope = build_diff_analyze_envelope(&output, &git_base, &git_head);

    let mut output_packets = vec![KernelPacket {
        packet_id: Some(format!(
            "diffy-{}",
            envelope.hash.chars().take(12).collect::<String>()
        )),
        format: default_packet_format(),
        body: serde_json::to_value(&envelope).map_err(|source| KernelError::ReducerFailed {
            target: ctx.target.clone(),
            detail: source.to_string(),
        })?,
        token_usage: Some(envelope.budget_cost.est_tokens),
        runtime_ms: Some(envelope.budget_cost.runtime_ms),
        metadata: json!({
            "reducer": "diffy.analyze",
            "kind": "diff_analyze",
            "hash": envelope.hash,
            "passed": output.gate_result.passed,
        }),
    }];

    if let Some(task_id) = ctx
        .policy_context
        .get("task_id")
        .and_then(Value::as_str)
        .filter(|task_id| !task_id.trim().is_empty())
    {
        let mut changed_paths = output
            .changed_line_context
            .changed_paths
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        changed_paths.sort();

        let clear_focus = suite_packet_core::AgentStateEventPayload {
            task_id: task_id.to_string(),
            event_id: format!("system-diff-focus-clear-{}", envelope.hash),
            occurred_at_unix: envelope.provenance.generated_at_unix,
            actor: "system:diffy.analyze".to_string(),
            paths: Vec::new(),
            symbols: Vec::new(),
            kind: suite_packet_core::AgentStateEventKind::FocusCleared,
            data: suite_packet_core::AgentStateEventData::FocusCleared { clear_all: true },
        };
        validate_agent_state_event(&clear_focus)
            .map_err(|detail| KernelError::InvalidRequest { detail })?;
        let (_, focus_clear_packet) =
            build_agent_state_packet(&ctx.target, &clear_focus, "diffy.analyze")?;
        output_packets.push(focus_clear_packet);

        if !changed_paths.is_empty() {
            let focus_event = suite_packet_core::AgentStateEventPayload {
                task_id: task_id.to_string(),
                event_id: format!("system-diff-focus-set-{}", envelope.hash),
                occurred_at_unix: envelope.provenance.generated_at_unix,
                actor: "system:diffy.analyze".to_string(),
                paths: changed_paths.clone(),
                symbols: Vec::new(),
                kind: suite_packet_core::AgentStateEventKind::FocusSet,
                data: suite_packet_core::AgentStateEventData::FocusSet { note: None },
            };
            validate_agent_state_event(&focus_event)
                .map_err(|detail| KernelError::InvalidRequest { detail })?;
            let (_, focus_packet) =
                build_agent_state_packet(&ctx.target, &focus_event, "diffy.analyze")?;
            output_packets.push(focus_packet);
        }

        let step_event = suite_packet_core::AgentStateEventPayload {
            task_id: task_id.to_string(),
            event_id: format!("system-diff-step-{}", envelope.hash),
            occurred_at_unix: envelope.provenance.generated_at_unix,
            actor: "system:diffy.analyze".to_string(),
            paths: changed_paths,
            symbols: Vec::new(),
            kind: suite_packet_core::AgentStateEventKind::StepCompleted,
            data: suite_packet_core::AgentStateEventData::StepCompleted {
                step_id: "diff.analyze".to_string(),
            },
        };
        validate_agent_state_event(&step_event)
            .map_err(|detail| KernelError::InvalidRequest { detail })?;
        let (_, step_packet) = build_agent_state_packet(&ctx.target, &step_event, "diffy.analyze")?;
        output_packets.push(step_packet);

        ctx.set_shared("task_id", Value::String(task_id.to_string()));
    }

    Ok(ReducerResult {
        output_packets,
        metadata: json!({
            "reducer": "diffy.analyze",
            "kind": "diff_analyze",
            "passed": output.gate_result.passed,
        }),
    })
}

pub(crate) fn run_testy_impact_reducer(
    ctx: &mut ExecutionContext,
    _input_packets: &[KernelPacket],
) -> Result<ReducerResult, KernelError> {
    let input: ImpactKernelInput =
        serde_json::from_value(ctx.reducer_input.clone()).map_err(|source| {
            KernelError::ReducerFailed {
                target: ctx.target.clone(),
                detail: format!("invalid reducer input: {source}"),
            }
        })?;

    let testmap_path = input.testmap.clone();
    let git_base = input.base.clone();
    let git_head = input.head.clone();
    let adapters = testy_cli_common::adapters::default_impact_adapters();
    let output = testy_core::command_impact::run_legacy_impact(
        testy_core::command_impact::LegacyImpactArgs {
            base: input.base.clone(),
            head: input.head.clone(),
            testmap: input.testmap.clone(),
            print_command: input.print_command,
        },
        &input.config_path,
        &adapters,
    )
    .map_err(|source| KernelError::ReducerFailed {
        target: ctx.target.clone(),
        detail: source.to_string(),
    })?;

    let envelope = build_test_impact_envelope(
        &output,
        &testmap_path,
        git_base.as_deref(),
        git_head.as_deref(),
    );

    Ok(ReducerResult {
        output_packets: vec![KernelPacket {
            packet_id: Some(format!(
                "testy-{}",
                envelope.hash.chars().take(12).collect::<String>()
            )),
            format: default_packet_format(),
            body: serde_json::to_value(&envelope).map_err(|source| KernelError::ReducerFailed {
                target: ctx.target.clone(),
                detail: source.to_string(),
            })?,
            token_usage: Some(envelope.budget_cost.est_tokens),
            runtime_ms: Some(envelope.budget_cost.runtime_ms),
            metadata: json!({
                "reducer": "testy.impact",
                "kind": "test_impact",
                "hash": envelope.hash,
                "selected_tests": output.result.selected_tests.len(),
            }),
        }],
        metadata: json!({
            "reducer": "testy.impact",
            "kind": "test_impact",
            "selected_tests": output.result.selected_tests.len(),
        }),
    })
}

pub(crate) fn run_stacky_slice(
    ctx: &mut ExecutionContext,
    _input_packets: &[KernelPacket],
) -> Result<ReducerResult, KernelError> {
    let input: stacky_core::StackSliceRequest = serde_json::from_value(ctx.reducer_input.clone())
        .map_err(|source| KernelError::ReducerFailed {
        target: ctx.target.clone(),
        detail: format!("invalid reducer input: {source}"),
    })?;

    let agent_snapshot = load_agent_snapshot(ctx)?;
    let envelope = stacky_core::slice_to_envelope(input);
    let payload = envelope.payload.clone();
    let focus_paths = agent_snapshot
        .as_ref()
        .map(|snapshot| snapshot.focus_paths.clone())
        .unwrap_or_default();
    let mut matching_files = BTreeSet::new();
    let mut unrelated_files = BTreeSet::new();
    for file in envelope.files.iter().map(|file| file.path.as_str()) {
        if path_matches_any(&focus_paths, file) {
            matching_files.insert(file.to_string());
        } else {
            unrelated_files.insert(file.to_string());
        }
    }

    let kernel_packet = KernelPacket {
        packet_id: Some(format!(
            "stacky-{}",
            envelope.hash.chars().take(12).collect::<String>()
        )),
        format: default_packet_format(),
        body: serde_json::to_value(&envelope).map_err(|source| KernelError::ReducerFailed {
            target: ctx.target.clone(),
            detail: source.to_string(),
        })?,
        token_usage: Some(envelope.budget_cost.est_tokens),
        runtime_ms: Some(envelope.budget_cost.runtime_ms),
        metadata: json!({
            "tool": envelope.tool,
            "reducer": "slice",
            "kind": envelope.kind,
            "hash": envelope.hash,
            "unique_failures": payload.unique_failures,
            "duplicates_removed": payload.duplicates_removed,
            "task_state": {
                "focus_paths": focus_paths,
                "matching_files": matching_files,
                "unrelated_files": unrelated_files,
            },
        }),
    };

    Ok(ReducerResult {
        output_packets: vec![kernel_packet],
        metadata: json!({
            "reducer": "stacky.slice",
            "kind": "stack_slice",
            "unique_failures": payload.unique_failures,
            "duplicates_removed": payload.duplicates_removed,
            "task_state": {
                "matching_failures": matching_files.len(),
                "unrelated_failures": unrelated_files.len(),
            },
        }),
    })
}

pub(crate) fn run_buildy_reduce(
    ctx: &mut ExecutionContext,
    _input_packets: &[KernelPacket],
) -> Result<ReducerResult, KernelError> {
    let input: buildy_core::BuildReduceRequest = serde_json::from_value(ctx.reducer_input.clone())
        .map_err(|source| KernelError::ReducerFailed {
            target: ctx.target.clone(),
            detail: format!("invalid reducer input: {source}"),
        })?;

    let envelope = buildy_core::reduce_to_envelope(input);
    let payload = envelope.payload.clone();

    let kernel_packet = KernelPacket {
        packet_id: Some(format!(
            "buildy-{}",
            envelope.hash.chars().take(12).collect::<String>()
        )),
        format: default_packet_format(),
        body: serde_json::to_value(&envelope).map_err(|source| KernelError::ReducerFailed {
            target: ctx.target.clone(),
            detail: source.to_string(),
        })?,
        token_usage: Some(envelope.budget_cost.est_tokens),
        runtime_ms: Some(envelope.budget_cost.runtime_ms),
        metadata: json!({
            "tool": envelope.tool,
            "reducer": "reduce",
            "kind": envelope.kind,
            "hash": envelope.hash,
            "unique_diagnostics": payload.unique_diagnostics,
            "duplicates_removed": payload.duplicates_removed,
        }),
    };

    Ok(ReducerResult {
        output_packets: vec![kernel_packet],
        metadata: json!({
            "reducer": "buildy.reduce",
            "kind": "build_reduce",
            "unique_diagnostics": payload.unique_diagnostics,
            "duplicates_removed": payload.duplicates_removed,
        }),
    })
}

pub(crate) fn run_proxy_run(
    ctx: &mut ExecutionContext,
    _input_packets: &[KernelPacket],
) -> Result<ReducerResult, KernelError> {
    let input: suite_proxy_core::ProxyRunRequest =
        serde_json::from_value(ctx.reducer_input.clone()).map_err(|source| {
            KernelError::ReducerFailed {
                target: ctx.target.clone(),
                detail: format!("invalid reducer input: {source}"),
            }
        })?;

    let envelope =
        suite_proxy_core::run_and_reduce(input).map_err(|source| KernelError::ReducerFailed {
            target: ctx.target.clone(),
            detail: source.to_string(),
        })?;

    let kernel_packet = KernelPacket {
        packet_id: Some(format!(
            "proxy-{}",
            envelope.hash.chars().take(12).collect::<String>()
        )),
        format: default_packet_format(),
        body: serde_json::to_value(&envelope).map_err(|source| KernelError::ReducerFailed {
            target: ctx.target.clone(),
            detail: source.to_string(),
        })?,
        token_usage: Some(envelope.budget_cost.est_tokens),
        runtime_ms: Some(envelope.budget_cost.runtime_ms),
        metadata: json!({
            "tool": "proxy",
            "reducer": "run",
            "kind": envelope.kind,
            "hash": envelope.hash,
            "lines_out": envelope.payload.lines_out,
            "bytes_saved": envelope.payload.bytes_saved,
        }),
    };

    Ok(ReducerResult {
        output_packets: vec![kernel_packet],
        metadata: json!({
            "reducer": "proxy.run",
            "kind": "command_summary",
        }),
    })
}

pub(crate) fn run_mapy_repo(
    ctx: &mut ExecutionContext,
    _input_packets: &[KernelPacket],
) -> Result<ReducerResult, KernelError> {
    let mut input: mapy_core::RepoMapRequest = serde_json::from_value(ctx.reducer_input.clone())
        .map_err(|source| KernelError::ReducerFailed {
            target: ctx.target.clone(),
            detail: format!("invalid reducer input: {source}"),
        })?;
    if let Some(snapshot) = load_agent_snapshot(ctx)? {
        for path in snapshot.focus_paths {
            if !input.focus_paths.iter().any(|existing| existing == &path) {
                input.focus_paths.push(path);
            }
        }
        for symbol in snapshot.focus_symbols {
            if !input
                .focus_symbols
                .iter()
                .any(|existing| existing == &symbol)
            {
                input.focus_symbols.push(symbol);
            }
        }
    }
    let effective_focus_paths = input.focus_paths.clone();
    let effective_focus_symbols = input.focus_symbols.clone();

    let envelope =
        mapy_core::build_repo_map(input).map_err(|source| KernelError::ReducerFailed {
            target: ctx.target.clone(),
            detail: source.to_string(),
        })?;

    let kernel_packet = KernelPacket {
        packet_id: Some(format!(
            "mapy-{}",
            envelope.hash.chars().take(12).collect::<String>()
        )),
        format: default_packet_format(),
        body: serde_json::to_value(&envelope).map_err(|source| KernelError::ReducerFailed {
            target: ctx.target.clone(),
            detail: source.to_string(),
        })?,
        token_usage: Some(envelope.budget_cost.est_tokens),
        runtime_ms: Some(envelope.budget_cost.runtime_ms),
        metadata: json!({
            "tool": "mapy",
            "reducer": "repo",
            "kind": envelope.kind,
            "hash": envelope.hash,
            "files_ranked": envelope.payload.files_ranked.len(),
            "symbols_ranked": envelope.payload.symbols_ranked.len(),
            "focus_paths": effective_focus_paths,
            "focus_symbols": effective_focus_symbols,
            "focus_hits": envelope.payload.focus_hits.clone(),
        }),
    };

    Ok(ReducerResult {
        output_packets: vec![kernel_packet],
        metadata: json!({
            "reducer": "mapy.repo",
            "kind": "repo_map",
            "focus_paths": effective_focus_paths,
            "focus_symbols": effective_focus_symbols,
            "focus_hits": envelope.payload.focus_hits.clone(),
        }),
    })
}
