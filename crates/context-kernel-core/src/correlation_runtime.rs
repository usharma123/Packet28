use std::path::PathBuf;

use serde::de::DeserializeOwned;

use super::*;

pub(crate) fn build_context_correlation_packet(
    target: &str,
    task_id: Option<String>,
    findings: Vec<suite_packet_core::ContextCorrelationFinding>,
    debug: Option<Value>,
) -> Result<
    (
        suite_packet_core::EnvelopeV1<suite_packet_core::ContextCorrelationPayload>,
        KernelPacket,
    ),
    KernelError,
> {
    let payload = suite_packet_core::ContextCorrelationPayload {
        task_id,
        finding_count: findings.len(),
        findings,
        debug,
    };
    let payload_bytes = serde_json::to_vec(&payload).unwrap_or_default().len();
    let mut files = BTreeSet::new();
    let mut symbols = BTreeSet::new();
    for finding in &payload.findings {
        for evidence in &finding.evidence_refs {
            if evidence.kind == "file" {
                files.insert(evidence.value.clone());
            } else {
                symbols.insert((evidence.kind.clone(), evidence.value.clone()));
            }
        }
    }

    let summary = if payload.findings.is_empty() {
        "correlation findings=0".to_string()
    } else {
        let preview = payload
            .findings
            .iter()
            .take(3)
            .map(|finding| finding.summary.clone())
            .collect::<Vec<_>>()
            .join(" | ");
        format!(
            "correlation findings={} :: {preview}",
            payload.findings.len()
        )
    };

    let envelope = suite_packet_core::EnvelopeV1 {
        version: "1".to_string(),
        tool: "contextq".to_string(),
        kind: "context_correlate".to_string(),
        hash: String::new(),
        summary,
        files: files
            .into_iter()
            .map(|path| suite_packet_core::FileRef {
                path,
                relevance: Some(1.0),
                source: Some("contextq.correlate".to_string()),
            })
            .collect(),
        symbols: symbols
            .into_iter()
            .map(|(kind, name)| suite_packet_core::SymbolRef {
                name,
                file: None,
                kind: Some(kind),
                relevance: Some(1.0),
                source: Some("contextq.correlate".to_string()),
            })
            .collect(),
        risk: None,
        confidence: Some(if payload.findings.is_empty() {
            1.0
        } else {
            0.85
        }),
        budget_cost: suite_packet_core::BudgetCost {
            est_tokens: 0,
            est_bytes: 0,
            runtime_ms: 0,
            tool_calls: 1,
            payload_est_tokens: Some((payload_bytes / 4) as u64),
            payload_est_bytes: Some(payload_bytes),
        },
        provenance: suite_packet_core::Provenance {
            inputs: payload
                .task_id
                .as_ref()
                .map(|task_id| vec![format!("task:{task_id}")])
                .unwrap_or_default(),
            git_base: None,
            git_head: None,
            generated_at_unix: now_unix(),
        },
        payload,
    }
    .with_canonical_hash_and_real_budget();

    let packet = KernelPacket {
        packet_id: Some(format!(
            "contextq-correlate-{}",
            envelope.hash.chars().take(12).collect::<String>()
        )),
        format: default_packet_format(),
        body: serde_json::to_value(&envelope).map_err(|source| KernelError::ReducerFailed {
            target: target.to_string(),
            detail: source.to_string(),
        })?,
        token_usage: Some(envelope.budget_cost.est_tokens),
        runtime_ms: Some(envelope.budget_cost.runtime_ms),
        metadata: json!({
            "reducer": "contextq.correlate",
            "kind": "context_correlate",
            "hash": envelope.hash,
            "finding_count": envelope.payload.finding_count,
        }),
    };

    Ok((envelope, packet))
}

#[derive(Clone)]
struct CorrelatablePacket<T> {
    packet_id: Option<String>,
    packet_type: &'static str,
    envelope: suite_packet_core::EnvelopeV1<T>,
}

fn parse_correlatable_packet<T: DeserializeOwned + Default>(
    packet: &KernelPacket,
    packet_type: &'static str,
    tool: &str,
    kind: &str,
) -> Option<CorrelatablePacket<T>> {
    let value = extract_packet_value(&packet.body);
    let envelope = serde_json::from_value::<suite_packet_core::EnvelopeV1<T>>(value).ok()?;
    (envelope.tool == tool && envelope.kind == kind).then_some(CorrelatablePacket {
        packet_id: packet.packet_id.clone(),
        packet_type,
        envelope,
    })
}

fn diff_changed_files(packet: &CorrelatablePacket<DiffAnalyzeKernelOutput>) -> BTreeSet<String> {
    let workspace_root = kernel_workspace_root();
    let from_payload = packet
        .envelope
        .payload
        .diffs
        .iter()
        .filter_map(|diff| normalize_context_path(&diff.path, workspace_root.as_deref()))
        .map(|path| path.canonical)
        .collect::<BTreeSet<_>>();
    if from_payload.is_empty() {
        packet
            .envelope
            .files
            .iter()
            .filter_map(|file| normalize_context_path(&file.path, workspace_root.as_deref()))
            .map(|path| path.canonical)
            .collect()
    } else {
        from_payload
    }
}

fn packet_files<T>(packet: &CorrelatablePacket<T>) -> BTreeSet<String> {
    let workspace_root = kernel_workspace_root();
    packet
        .envelope
        .files
        .iter()
        .filter_map(|file| normalize_context_path(&file.path, workspace_root.as_deref()))
        .map(|path| path.canonical)
        .collect()
}

fn map_has_edge(
    packet: &CorrelatablePacket<mapy_core::RepoMapPayload>,
    left: &BTreeSet<String>,
    right: &BTreeSet<String>,
) -> bool {
    let workspace_root = kernel_workspace_root();
    packet.envelope.payload.edges.iter().any(|edge| {
        let Some(from) = packet
            .envelope
            .files
            .get(edge.from_file_idx)
            .and_then(|file| normalize_context_path(&file.path, workspace_root.as_deref()))
            .map(|path| path.canonical)
        else {
            return false;
        };
        let Some(to) = packet
            .envelope
            .files
            .get(edge.to_file_idx)
            .and_then(|file| normalize_context_path(&file.path, workspace_root.as_deref()))
            .map(|path| path.canonical)
        else {
            return false;
        };
        (left.contains(&from) && right.contains(&to))
            || (left.contains(&to) && right.contains(&from))
    })
}

fn evidence_file_refs(
    packet_id: &Option<String>,
    packet_type: &str,
    values: impl IntoIterator<Item = String>,
) -> Vec<suite_packet_core::CorrelationEvidenceRef> {
    values
        .into_iter()
        .map(|value| suite_packet_core::CorrelationEvidenceRef {
            packet_id: packet_id.clone(),
            packet_type: packet_type.to_string(),
            kind: "file".to_string(),
            value,
        })
        .collect()
}

#[derive(Clone, Default)]
struct NormalizedCorrelationPacket {
    packet_id: Option<String>,
    packet_type: String,
    tool: String,
    kind: String,
    files: BTreeSet<String>,
    file_basenames: BTreeSet<String>,
    symbols: BTreeSet<String>,
    tests: BTreeSet<String>,
    map_edges: Vec<(String, String)>,
}

fn packet_type_for(tool: &str, kind: &str) -> String {
    match (tool, kind) {
        ("diffy", "diff_analyze") => suite_packet_core::PACKET_TYPE_DIFF_ANALYZE.to_string(),
        ("testy", "test_impact") => suite_packet_core::PACKET_TYPE_TEST_IMPACT.to_string(),
        ("stacky", "stack_slice") => suite_packet_core::PACKET_TYPE_STACK_SLICE.to_string(),
        ("buildy", "build_reduce") => suite_packet_core::PACKET_TYPE_BUILD_REDUCE.to_string(),
        ("mapy", "repo_map") => suite_packet_core::PACKET_TYPE_MAP_REPO.to_string(),
        ("agenty", "agent_snapshot") => suite_packet_core::PACKET_TYPE_AGENT_SNAPSHOT.to_string(),
        ("agenty", "agent_state") => suite_packet_core::PACKET_TYPE_AGENT_STATE.to_string(),
        ("contextq", "context_correlate") => {
            suite_packet_core::PACKET_TYPE_CONTEXT_CORRELATE.to_string()
        }
        ("contextq", "context_manage") => suite_packet_core::PACKET_TYPE_CONTEXT_MANAGE.to_string(),
        ("contextq", "context_assemble") => {
            suite_packet_core::PACKET_TYPE_CONTEXT_ASSEMBLE.to_string()
        }
        _ => format!("suite.{tool}.{kind}.v1"),
    }
}

pub(crate) fn kernel_workspace_root() -> Option<PathBuf> {
    std::env::current_dir().ok()
}

fn normalize_correlation_packet(packet: &KernelPacket) -> Option<NormalizedCorrelationPacket> {
    let value = extract_packet_value(&packet.body);
    let tool = value.get("tool").and_then(Value::as_str)?.to_string();
    let kind = value.get("kind").and_then(Value::as_str)?.to_string();
    let packet_type = packet_type_for(&tool, &kind);
    let workspace_root = kernel_workspace_root();
    let mut files = BTreeSet::new();
    let mut file_basenames = BTreeSet::new();
    let mut symbols = BTreeSet::new();
    let mut tests = BTreeSet::new();
    let mut map_edges = Vec::new();

    if let Some(file_refs) = value.get("files").and_then(Value::as_array) {
        for file in file_refs {
            if let Some(path) = file.get("path").and_then(Value::as_str) {
                if let Some(path) = normalize_context_path(path, workspace_root.as_deref()) {
                    files.insert(path.canonical.clone());
                    if let Some(basename) = path.basename {
                        file_basenames.insert(basename);
                    }
                }
            }
        }
    }
    if let Some(symbol_refs) = value.get("symbols").and_then(Value::as_array) {
        for symbol in symbol_refs {
            if let Some(name) = symbol.get("name").and_then(Value::as_str) {
                symbols.insert(name.to_string());
            }
        }
    }
    collect_packet_refs(
        value.get("payload").unwrap_or(&value),
        workspace_root.as_deref(),
        &mut files,
        &mut symbols,
        &mut tests,
    );

    if tool == "mapy" && kind == "repo_map" {
        if let (Some(payload), Some(file_refs)) = (
            value.get("payload").and_then(Value::as_object),
            value.get("files").and_then(Value::as_array),
        ) {
            if let Some(edges) = payload.get("edges").and_then(Value::as_array) {
                for edge in edges {
                    let Some(from_idx) = edge.get("from_file_idx").and_then(Value::as_u64) else {
                        continue;
                    };
                    let Some(to_idx) = edge.get("to_file_idx").and_then(Value::as_u64) else {
                        continue;
                    };
                    let Some(from) = file_refs
                        .get(from_idx as usize)
                        .and_then(|file| file.get("path"))
                        .and_then(Value::as_str)
                        .and_then(|path| normalize_context_path(path, workspace_root.as_deref()))
                    else {
                        continue;
                    };
                    let Some(to) = file_refs
                        .get(to_idx as usize)
                        .and_then(|file| file.get("path"))
                        .and_then(Value::as_str)
                        .and_then(|path| normalize_context_path(path, workspace_root.as_deref()))
                    else {
                        continue;
                    };
                    map_edges.push((from.canonical, to.canonical));
                }
            }
        }
    }

    Some(NormalizedCorrelationPacket {
        packet_id: packet.packet_id.clone(),
        packet_type,
        tool,
        kind,
        files,
        file_basenames,
        symbols,
        tests,
        map_edges,
    })
}

pub(crate) fn collect_packet_refs(
    value: &Value,
    workspace_root: Option<&Path>,
    files: &mut BTreeSet<String>,
    symbols: &mut BTreeSet<String>,
    tests: &mut BTreeSet<String>,
) {
    match value {
        Value::Object(map) => {
            if let Some(path) = map.get("path").and_then(Value::as_str) {
                if let Some(path) = normalize_context_path(path, workspace_root) {
                    files.insert(path.canonical);
                }
            }
            if let Some(file) = map.get("file").and_then(Value::as_str) {
                if let Some(path) = normalize_context_path(file, workspace_root) {
                    files.insert(path.canonical);
                }
            }
            if let Some(name) = map.get("name").and_then(Value::as_str) {
                symbols.insert(name.to_ascii_lowercase());
            }
            if let Some(test_id) = map.get("test_id").and_then(Value::as_str) {
                tests.insert(test_id.to_ascii_lowercase());
            }
            if map
                .get("kind")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind.eq_ignore_ascii_case("file"))
            {
                if let Some(value) = map.get("value").and_then(Value::as_str) {
                    if let Some(path) = normalize_context_path(value, workspace_root) {
                        files.insert(path.canonical);
                    }
                }
            }
            if map
                .get("kind")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind.eq_ignore_ascii_case("symbol"))
            {
                if let Some(value) = map.get("value").and_then(Value::as_str) {
                    symbols.insert(value.to_ascii_lowercase());
                }
            }
            if let Some(selected_tests) = map.get("selected_tests").and_then(Value::as_array) {
                for test in selected_tests {
                    if let Some(test) = test.as_str() {
                        tests.insert(test.to_ascii_lowercase());
                    }
                }
            }
            for child in map.values() {
                collect_packet_refs(child, workspace_root, files, symbols, tests);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_packet_refs(item, workspace_root, files, symbols, tests);
            }
        }
        Value::String(text) => {
            if let Some(path) = normalize_context_path(text, workspace_root) {
                files.insert(path.canonical);
            }
        }
        _ => {}
    }
}

fn dedupe_findings(
    findings: Vec<suite_packet_core::ContextCorrelationFinding>,
) -> Vec<suite_packet_core::ContextCorrelationFinding> {
    let mut deduped = HashMap::<String, suite_packet_core::ContextCorrelationFinding>::new();
    for finding in findings {
        let mut evidence = finding
            .evidence_refs
            .iter()
            .map(|item| format!("{}:{}:{}", item.packet_type, item.kind, item.value))
            .collect::<Vec<_>>();
        evidence.sort();
        let key = format!(
            "{}|{}|{}",
            finding.rule,
            finding.relation,
            evidence.join("|")
        );
        deduped.entry(key).or_insert(finding);
    }
    let mut values = deduped.into_values().collect::<Vec<_>>();
    values.sort_by(|a, b| a.rule.cmp(&b.rule).then_with(|| a.summary.cmp(&b.summary)));
    values
}

pub(crate) fn correlate_packets(
    input_packets: &[KernelPacket],
    task_id: Option<String>,
    snapshot: Option<&suite_packet_core::AgentSnapshotPayload>,
) -> Vec<suite_packet_core::ContextCorrelationFinding> {
    let diffs = input_packets
        .iter()
        .filter_map(|packet| {
            parse_correlatable_packet::<DiffAnalyzeKernelOutput>(
                packet,
                suite_packet_core::PACKET_TYPE_DIFF_ANALYZE,
                "diffy",
                "diff_analyze",
            )
        })
        .collect::<Vec<_>>();
    let impacts = input_packets
        .iter()
        .filter_map(|packet| {
            parse_correlatable_packet::<ImpactKernelOutput>(
                packet,
                suite_packet_core::PACKET_TYPE_TEST_IMPACT,
                "testy",
                "test_impact",
            )
        })
        .collect::<Vec<_>>();
    let stacks = input_packets
        .iter()
        .filter_map(|packet| {
            parse_correlatable_packet::<stacky_core::StackSliceOutput>(
                packet,
                suite_packet_core::PACKET_TYPE_STACK_SLICE,
                "stacky",
                "stack_slice",
            )
        })
        .collect::<Vec<_>>();
    let builds = input_packets
        .iter()
        .filter_map(|packet| {
            parse_correlatable_packet::<buildy_core::BuildReduceOutput>(
                packet,
                suite_packet_core::PACKET_TYPE_BUILD_REDUCE,
                "buildy",
                "build_reduce",
            )
        })
        .collect::<Vec<_>>();
    let maps = input_packets
        .iter()
        .filter_map(|packet| {
            parse_correlatable_packet::<mapy_core::RepoMapPayload>(
                packet,
                suite_packet_core::PACKET_TYPE_MAP_REPO,
                "mapy",
                "repo_map",
            )
        })
        .collect::<Vec<_>>();

    let mut findings = Vec::new();

    for diff in &diffs {
        let changed = diff_changed_files(diff);
        if changed.is_empty() {
            continue;
        }

        for stack in &stacks {
            let stack_files = packet_files(stack);
            if stack_files.is_empty() {
                continue;
            }
            let overlap = changed
                .intersection(&stack_files)
                .cloned()
                .collect::<Vec<_>>();
            let map_connected = maps
                .iter()
                .any(|map| map_has_edge(map, &changed, &stack_files));
            let relation = if !overlap.is_empty() {
                "related"
            } else if !map_connected && !maps.is_empty() {
                "unrelated"
            } else {
                "possibly_related"
            };
            let summary = if !overlap.is_empty() {
                format!("Stack failures touch changed files: {}", overlap.join(", "))
            } else if relation == "unrelated" {
                format!(
                    "Stack failures in {} appear unrelated to diff in {}",
                    stack_files.iter().cloned().collect::<Vec<_>>().join(", "),
                    changed.iter().cloned().collect::<Vec<_>>().join(", ")
                )
            } else {
                format!(
                    "Stack failures in {} may relate indirectly to diff in {}",
                    stack_files.iter().cloned().collect::<Vec<_>>().join(", "),
                    changed.iter().cloned().collect::<Vec<_>>().join(", ")
                )
            };
            let mut evidence_refs =
                evidence_file_refs(&diff.packet_id, diff.packet_type, changed.clone());
            evidence_refs.extend(evidence_file_refs(
                &stack.packet_id,
                stack.packet_type,
                stack_files.clone(),
            ));
            findings.push(suite_packet_core::ContextCorrelationFinding {
                rule: "diff_vs_stack".to_string(),
                relation: relation.to_string(),
                confidence: if relation == "related" {
                    0.92
                } else if relation == "unrelated" {
                    0.86
                } else {
                    0.62
                },
                summary,
                evidence_refs,
            });
        }

        for impact in &impacts {
            if impact.envelope.payload.result.selected_tests.is_empty() {
                continue;
            }
            let mut evidence_refs =
                evidence_file_refs(&diff.packet_id, diff.packet_type, changed.clone());
            evidence_refs.extend(impact.envelope.payload.result.selected_tests.iter().map(
                |test_id: &String| suite_packet_core::CorrelationEvidenceRef {
                    packet_id: impact.packet_id.clone(),
                    packet_type: impact.packet_type.to_string(),
                    kind: "test".to_string(),
                    value: test_id.clone(),
                },
            ));
            findings.push(suite_packet_core::ContextCorrelationFinding {
                rule: "diff_vs_impact".to_string(),
                relation: "supports".to_string(),
                confidence: 0.78,
                summary: format!(
                    "Test impact selected {} tests for changed files {}",
                    impact.envelope.payload.result.selected_tests.len(),
                    changed.iter().cloned().collect::<Vec<_>>().join(", ")
                ),
                evidence_refs,
            });
        }

        for build in &builds {
            let build_files = packet_files(build);
            let untouched = build_files
                .difference(&changed)
                .cloned()
                .collect::<Vec<_>>();
            if untouched.is_empty() {
                continue;
            }
            let mut evidence_refs =
                evidence_file_refs(&diff.packet_id, diff.packet_type, changed.clone());
            evidence_refs.extend(evidence_file_refs(
                &build.packet_id,
                build.packet_type,
                untouched.clone(),
            ));
            findings.push(suite_packet_core::ContextCorrelationFinding {
                rule: "diff_vs_build".to_string(),
                relation: "pre_existing_or_unrelated".to_string(),
                confidence: 0.84,
                summary: format!(
                    "Build diagnostics touch untouched files: {}",
                    untouched.join(", ")
                ),
                evidence_refs,
            });
        }
    }

    let normalized = input_packets
        .iter()
        .filter_map(normalize_correlation_packet)
        .collect::<Vec<_>>();
    let basename_counts = normalized
        .iter()
        .flat_map(|packet| packet.file_basenames.iter().cloned())
        .fold(HashMap::<String, usize>::new(), |mut counts, basename| {
            *counts.entry(basename).or_insert(0) += 1;
            counts
        });

    for (idx, left) in normalized.iter().enumerate() {
        for right in normalized.iter().skip(idx + 1) {
            let shared_files = left
                .files
                .intersection(&right.files)
                .cloned()
                .collect::<Vec<_>>();
            let shared_basenames = if shared_files.is_empty() {
                left.file_basenames
                    .intersection(&right.file_basenames)
                    .filter(|basename| basename_counts.get(*basename) == Some(&2))
                    .cloned()
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            if !shared_files.is_empty() || !shared_basenames.is_empty() {
                let mut evidence_refs =
                    evidence_file_refs(&left.packet_id, &left.packet_type, shared_files.clone());
                evidence_refs.extend(evidence_file_refs(
                    &right.packet_id,
                    &right.packet_type,
                    shared_files.clone(),
                ));
                if shared_files.is_empty() {
                    evidence_refs.extend(shared_basenames.iter().map(|value| {
                        suite_packet_core::CorrelationEvidenceRef {
                            packet_id: left.packet_id.clone(),
                            packet_type: left.packet_type.clone(),
                            kind: "file_basename".to_string(),
                            value: value.clone(),
                        }
                    }));
                    evidence_refs.extend(shared_basenames.iter().map(|value| {
                        suite_packet_core::CorrelationEvidenceRef {
                            packet_id: right.packet_id.clone(),
                            packet_type: right.packet_type.clone(),
                            kind: "file_basename".to_string(),
                            value: value.clone(),
                        }
                    }));
                }
                findings.push(suite_packet_core::ContextCorrelationFinding {
                    rule: "shared_file".to_string(),
                    relation: "related".to_string(),
                    confidence: if shared_files.is_empty() { 0.58 } else { 0.74 },
                    summary: if shared_files.is_empty() {
                        format!(
                            "Packets share unique file basenames: {}",
                            shared_basenames.join(", ")
                        )
                    } else {
                        format!("Packets share files: {}", shared_files.join(", "))
                    },
                    evidence_refs,
                });
            }

            let shared_symbols = left
                .symbols
                .intersection(&right.symbols)
                .cloned()
                .collect::<Vec<_>>();
            if !shared_symbols.is_empty() {
                let mut evidence_refs = shared_symbols
                    .iter()
                    .map(|value| suite_packet_core::CorrelationEvidenceRef {
                        packet_id: left.packet_id.clone(),
                        packet_type: left.packet_type.clone(),
                        kind: "symbol".to_string(),
                        value: value.clone(),
                    })
                    .collect::<Vec<_>>();
                evidence_refs.extend(shared_symbols.iter().map(|value| {
                    suite_packet_core::CorrelationEvidenceRef {
                        packet_id: right.packet_id.clone(),
                        packet_type: right.packet_type.clone(),
                        kind: "symbol".to_string(),
                        value: value.clone(),
                    }
                }));
                findings.push(suite_packet_core::ContextCorrelationFinding {
                    rule: "shared_symbol".to_string(),
                    relation: "related".to_string(),
                    confidence: 0.7,
                    summary: format!("Packets share symbols: {}", shared_symbols.join(", ")),
                    evidence_refs,
                });
            }

            let shared_tests = left
                .tests
                .intersection(&right.tests)
                .cloned()
                .collect::<Vec<_>>();
            if !shared_tests.is_empty() {
                let mut evidence_refs = shared_tests
                    .iter()
                    .map(|value| suite_packet_core::CorrelationEvidenceRef {
                        packet_id: left.packet_id.clone(),
                        packet_type: left.packet_type.clone(),
                        kind: "test".to_string(),
                        value: value.clone(),
                    })
                    .collect::<Vec<_>>();
                evidence_refs.extend(shared_tests.iter().map(|value| {
                    suite_packet_core::CorrelationEvidenceRef {
                        packet_id: right.packet_id.clone(),
                        packet_type: right.packet_type.clone(),
                        kind: "test".to_string(),
                        value: value.clone(),
                    }
                }));
                findings.push(suite_packet_core::ContextCorrelationFinding {
                    rule: "shared_test".to_string(),
                    relation: "related".to_string(),
                    confidence: 0.68,
                    summary: format!("Packets share tests: {}", shared_tests.join(", ")),
                    evidence_refs,
                });
            }

            for map in normalized
                .iter()
                .filter(|candidate| candidate.tool == "mapy" && candidate.kind == "repo_map")
            {
                let connected = map.map_edges.iter().any(|(from, to)| {
                    (left.files.contains(from) && right.files.contains(to))
                        || (left.files.contains(to) && right.files.contains(from))
                });
                if connected {
                    findings.push(suite_packet_core::ContextCorrelationFinding {
                        rule: "map_edge_connects".to_string(),
                        relation: "related".to_string(),
                        confidence: 0.76,
                        summary: format!(
                            "Repo map connects {} and {}",
                            left.packet_type, right.packet_type
                        ),
                        evidence_refs: vec![suite_packet_core::CorrelationEvidenceRef {
                            packet_id: map.packet_id.clone(),
                            packet_type: map.packet_type.clone(),
                            kind: "map_edge".to_string(),
                            value: "edge".to_string(),
                        }],
                    });
                }
            }
        }
    }

    if let Some(snapshot) = snapshot {
        let workspace_root = kernel_workspace_root();
        let snapshot_paths = snapshot
            .changed_paths_since_checkpoint
            .iter()
            .filter_map(|path| normalize_context_path(path, workspace_root.as_deref()))
            .map(|path| path.canonical)
            .collect::<Vec<_>>();
        let snapshot_basenames = snapshot
            .changed_paths_since_checkpoint
            .iter()
            .filter_map(|path| basename_alias(path))
            .collect::<BTreeSet<_>>();
        let snapshot_symbols = snapshot
            .changed_symbols_since_checkpoint
            .iter()
            .map(|symbol| symbol.to_ascii_lowercase())
            .collect::<Vec<_>>();
        for packet in &normalized {
            let shared_paths = packet
                .files
                .iter()
                .filter(|path| snapshot_paths.iter().any(|item| item == *path))
                .cloned()
                .collect::<Vec<_>>();
            let shared_path_basenames = if shared_paths.is_empty() {
                packet
                    .file_basenames
                    .iter()
                    .filter(|basename| snapshot_basenames.contains(*basename))
                    .filter(|basename| basename_counts.get(*basename) == Some(&1))
                    .cloned()
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            let shared_symbols = packet
                .symbols
                .iter()
                .filter(|symbol| snapshot_symbols.iter().any(|item| item == *symbol))
                .cloned()
                .collect::<Vec<_>>();
            if !shared_paths.is_empty()
                || !shared_path_basenames.is_empty()
                || !shared_symbols.is_empty()
            {
                let mut evidence_refs = shared_paths
                    .iter()
                    .map(|value| suite_packet_core::CorrelationEvidenceRef {
                        packet_id: packet.packet_id.clone(),
                        packet_type: packet.packet_type.clone(),
                        kind: "file".to_string(),
                        value: value.clone(),
                    })
                    .collect::<Vec<_>>();
                evidence_refs.extend(shared_symbols.iter().map(|value| {
                    suite_packet_core::CorrelationEvidenceRef {
                        packet_id: packet.packet_id.clone(),
                        packet_type: packet.packet_type.clone(),
                        kind: "symbol".to_string(),
                        value: value.clone(),
                    }
                }));
                evidence_refs.extend(shared_path_basenames.iter().map(|value| {
                    suite_packet_core::CorrelationEvidenceRef {
                        packet_id: packet.packet_id.clone(),
                        packet_type: packet.packet_type.clone(),
                        kind: "file_basename".to_string(),
                        value: value.clone(),
                    }
                }));
                findings.push(suite_packet_core::ContextCorrelationFinding {
                    rule: "task_focus_overlap".to_string(),
                    relation: "related".to_string(),
                    confidence: if shared_paths.is_empty() && !shared_path_basenames.is_empty() {
                        0.61
                    } else {
                        0.73
                    },
                    summary: format!(
                        "Packet overlaps task checkpoint deltas for task {}",
                        snapshot.task_id
                    ),
                    evidence_refs,
                });
            }
        }
    }

    if task_id.is_some() {
        dedupe_findings(findings)
    } else {
        dedupe_findings(findings)
    }
}
