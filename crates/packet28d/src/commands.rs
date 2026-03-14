use super::*;

pub(crate) fn run_cover_check(request: CoverCheckRequest) -> Result<CoverCheckResponse> {
    let config = if request.config_path.trim().is_empty() {
        suite_foundation_core::CovyConfig::default()
    } else {
        suite_foundation_core::CovyConfig::load(Path::new(&request.config_path))?
    };
    let base = request.base.as_deref().unwrap_or(&config.diff.base);
    let head = request.head.as_deref().unwrap_or(&config.diff.head);
    let issue_gate = suite_foundation_core::config::IssueGateConfig {
        max_new_errors: request.max_new_errors.or(config.gate.issues.max_new_errors),
        max_new_warnings: request
            .max_new_warnings
            .or(config.gate.issues.max_new_warnings),
        max_new_issues: config.gate.issues.max_new_issues,
    };
    let gate_config = suite_foundation_core::config::GateConfig {
        fail_under_total: request.fail_under_total.or(config.gate.fail_under_total),
        fail_under_changed: request
            .fail_under_changed
            .or(config.gate.fail_under_changed),
        fail_under_new: request.fail_under_new.or(config.gate.fail_under_new),
        issues: issue_gate,
    };
    let coverage_format = parse_format(&request.format)?;
    let source_root = request.source_root.as_ref().map(PathBuf::from);
    let strip_prefixes: Vec<String> = request
        .strip_prefix
        .iter()
        .cloned()
        .chain(config.ingest.strip_prefixes.iter().cloned())
        .collect();

    let mut coverage_paths = request.coverage.clone();
    coverage_paths.extend(request.paths.clone());
    let pipeline_request = diffy_core::pipeline::PipelineRequest {
        base: base.to_string(),
        head: head.to_string(),
        source_root,
        coverage: diffy_core::pipeline::PipelineCoverageInput {
            paths: coverage_paths,
            format: coverage_format,
            stdin: false,
            input_state_path: request.input.clone(),
            default_input_state_path: Some(".covy/state/latest.bin".to_string()),
            strip_prefixes,
            reject_paths_with_input: true,
            no_inputs_error:
                "No coverage files specified. Provide file paths, use --stdin, or run `covy ingest` first."
                    .to_string(),
        },
        diagnostics: diffy_core::pipeline::PipelineDiagnosticsInput {
            issue_patterns: request.issues.clone(),
            issues_state_path: request.issues_state.clone(),
            no_issues_state: request.no_issues_state,
            default_issues_state_path: ".covy/state/issues.bin".to_string(),
        },
        gate: gate_config,
    };
    let output = diffy_core::pipeline::run_analysis(
        pipeline_request,
        &diffy_core::pipeline::PipelineIngestAdapters {
            ingest_coverage_auto: |path| covy_ingest::ingest_path(path).map_err(Into::into),
            ingest_coverage_with_format: |path, format| {
                covy_ingest::ingest_path_with_format(path, format).map_err(Into::into)
            },
            ingest_coverage_stdin: |_format| {
                anyhow::bail!("stdin is not supported through packet28d")
            },
            ingest_diagnostics: |path| {
                covy_ingest::ingest_diagnostics_path(path).map_err(Into::into)
            },
        },
    )?;

    let gate_json = serde_json::to_value(&output.gate_result).unwrap_or_default();
    let gate_json_bytes = serde_json::to_vec(&gate_json).unwrap_or_default();
    let mut changed_paths = output
        .changed_line_context
        .changed_paths
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    changed_paths.sort();
    let envelope = suite_packet_core::EnvelopeV1 {
        version: "1".to_string(),
        tool: "covy".to_string(),
        kind: "coverage_gate".to_string(),
        hash: String::new(),
        summary: format!(
            "passed={} changed={:?} total={:?} new={:?}",
            output.gate_result.passed,
            output.gate_result.changed_coverage_pct,
            output.gate_result.total_coverage_pct,
            output.gate_result.new_file_coverage_pct
        ),
        files: changed_paths
            .iter()
            .map(|path: &String| suite_packet_core::FileRef {
                path: path.clone(),
                relevance: Some(0.75),
                source: Some("cover.check".to_string()),
            })
            .collect(),
        symbols: Vec::new(),
        risk: None,
        confidence: Some(if output.gate_result.passed { 1.0 } else { 0.8 }),
        budget_cost: suite_packet_core::BudgetCost {
            est_tokens: 0,
            est_bytes: 0,
            runtime_ms: 0,
            tool_calls: 1,
            payload_est_tokens: Some((gate_json_bytes.len() / 4) as u64),
            payload_est_bytes: Some(gate_json_bytes.len()),
        },
        provenance: suite_packet_core::Provenance {
            inputs: changed_paths,
            git_base: Some(base.to_string()),
            git_head: Some(head.to_string()),
            generated_at_unix: now_unix(),
        },
        payload: gate_json,
    }
    .with_canonical_hash_and_real_budget();

    Ok(CoverCheckResponse {
        exit_code: if output.gate_result.passed { 0 } else { 1 },
        packet_type: suite_packet_core::PACKET_TYPE_COVER_CHECK.to_string(),
        envelope: serde_json::from_value(serde_json::to_value(envelope)?)?,
    })
}

pub(crate) fn run_test_shard(request: TestShardRequest) -> Result<TestShardResponse> {
    if request.schema {
        return Ok(TestShardResponse {
            schema: Some(testy_core::command_shard::SHARD_PLAN_SCHEMA_EXAMPLES.to_string()),
            plan: None,
        });
    }

    let plan = testy_core::command_shard::run_shard_plan_command(
        testy_core::command_shard::ShardPlanArgs {
            shards: request.shards,
            tasks_json: request.tasks_json,
            tier: request.tier,
            include_tag: request.include_tag,
            exclude_tag: request.exclude_tag,
            tests_file: request.tests_file,
            impact_json: request.impact_json,
            timings: request.timings,
            unknown_test_seconds: request.unknown_test_seconds,
            algorithm: parse_shard_algorithm(request.algorithm.as_deref())?,
            write_files: request.write_files,
        },
        &request.config_path,
    )?;

    Ok(TestShardResponse {
        schema: None,
        plan: Some(plan),
    })
}

pub(crate) fn run_test_map(request: TestMapRequest) -> Result<TestMapResponse> {
    if request.schema {
        return Ok(TestMapResponse {
            schema: Some(testy_core::pipeline_testmap::TESTMAP_MANIFEST_SCHEMA_EXAMPLE.to_string()),
            warnings: Vec::new(),
            summary: None,
        });
    }

    let adapters = testy_core::pipeline_testmap::TestMapAdapters {
        ingest_coverage: |path| covy_ingest::ingest_path(path).map_err(Into::into),
    };
    let output = testy_core::command_testmap::run_testmap_build(
        testy_core::command_testmap::TestmapBuildArgs {
            manifest: request.manifest,
            output: request.output,
            timings_output: request.timings_output,
        },
        &adapters,
    )?;

    Ok(TestMapResponse {
        schema: None,
        warnings: output.warnings,
        summary: Some(TestMapSummary {
            manifest_files: output.summary.manifest_files,
            records: output.summary.records,
            tests: output.summary.tests,
            files: output.summary.files,
            output_testmap_path: output.summary.output_testmap_path,
            output_timings_path: output.summary.output_timings_path,
        }),
    })
}

pub(crate) fn run_context_store_list(
    request: ContextStoreListRequest,
) -> Result<ContextStoreListResponse> {
    let cache = load_cache_root(&request.root);
    let entries = cache.list_entries(
        &ContextStoreListFilter {
            target: request.target,
            contains_query: request.query,
            created_after_unix: request.created_after,
            created_before_unix: request.created_before,
        },
        &ContextStorePaging {
            offset: request.offset,
            limit: request.limit,
        },
    );
    Ok(ContextStoreListResponse { entries })
}

pub(crate) fn run_context_store_get(
    request: ContextStoreGetRequest,
) -> Result<ContextStoreGetResponse> {
    let cache = load_cache_root(&request.root);
    Ok(ContextStoreGetResponse {
        entry: cache.get_entry(&request.key),
    })
}

pub(crate) fn run_context_store_prune(
    request: ContextStorePruneDaemonRequest,
) -> Result<ContextStorePruneResponse> {
    let root = std::path::PathBuf::from(&request.root);
    let config = MemoryPersistConfig::new(root.clone());
    let mut cache = PacketCache::load_from_disk(&config);
    let report = cache.prune(ContextStorePruneRequest {
        all: request.all,
        ttl_secs: request.ttl_secs,
    });
    cache
        .save_to_disk(&config)
        .with_context(|| format!("failed to save context store at '{}'", root.display()))?;
    Ok(ContextStorePruneResponse { report })
}

pub(crate) fn run_context_store_stats(
    request: ContextStoreStatsRequest,
) -> Result<ContextStoreStatsResponse> {
    let cache = load_cache_root(&request.root);
    Ok(ContextStoreStatsResponse {
        stats: cache.stats(),
    })
}

pub(crate) fn run_context_recall(request: ContextRecallRequest) -> Result<ContextRecallResponse> {
    let cache = load_cache_root(&request.root);
    let now = now_unix();
    let since_default = now.saturating_sub(86_400);
    let scope = match request.scope.as_deref().unwrap_or_default() {
        "task_first" => context_memory_core::RecallScope::TaskFirst,
        "task_only" => context_memory_core::RecallScope::TaskOnly,
        _ if request.task_id.is_some() => context_memory_core::RecallScope::TaskFirst,
        _ => context_memory_core::RecallScope::Global,
    };
    let hits = cache.recall(
        &request.query,
        &RecallOptions {
            limit: request.limit,
            since_unix: request.since.or(Some(since_default)),
            until_unix: request.until,
            target: request.target,
            task_id: request.task_id,
            scope,
            packet_types: request.packet_types,
            path_filters: request.path_filters,
            symbol_filters: request.symbol_filters,
        },
    );
    Ok(ContextRecallResponse {
        query: request.query,
        hits,
    })
}

fn load_cache_root(root: &str) -> PacketCache {
    PacketCache::load_from_disk(&MemoryPersistConfig::new(std::path::PathBuf::from(root)))
}

fn parse_shard_algorithm(
    value: Option<&str>,
) -> Result<Option<testy_core::command_shard::PlannerAlgorithmArg>> {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        None => Ok(None),
        Some("lpt") => Ok(Some(testy_core::command_shard::PlannerAlgorithmArg::Lpt)),
        Some("whale-lpt") => Ok(Some(
            testy_core::command_shard::PlannerAlgorithmArg::WhaleLpt,
        )),
        Some(other) => Err(anyhow!(
            "unsupported shard algorithm '{other}'. Expected 'lpt' or 'whale-lpt'"
        )),
    }
}

fn parse_format(value: &str) -> Result<Option<CoverageFormat>> {
    match value.trim().to_ascii_lowercase().as_str() {
        "auto" => Ok(None),
        "lcov" => Ok(Some(CoverageFormat::Lcov)),
        "cobertura" => Ok(Some(CoverageFormat::Cobertura)),
        "jacoco" => Ok(Some(CoverageFormat::JaCoCo)),
        "gocov" => Ok(Some(CoverageFormat::GoCov)),
        "llvm-cov" | "llvmcov" => Ok(Some(CoverageFormat::LlvmCov)),
        other => Err(anyhow!("unsupported coverage format '{other}'")),
    }
}
