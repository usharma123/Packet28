pub mod error {
    pub use suite_packet_core::error::*;
}

use std::cmp::Reverse;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use suite_foundation_core::error::CovyError;

pub const DEFAULT_BUDGET_TOKENS: u64 = 5_000;
pub const DEFAULT_BUDGET_BYTES: usize = 32_000;
pub const CONTEXTQ_SCHEMA_VERSION: &str = "contextq.assemble.v1";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DetailMode {
    #[default]
    Compact,
    Rich,
}

#[derive(Debug, Clone)]
pub struct AssembleOptions {
    pub budget_tokens: u64,
    pub budget_bytes: usize,
    pub detail_mode: DetailMode,
    pub compact_assembly: bool,
    pub agent_snapshot: Option<suite_packet_core::AgentSnapshotPayload>,
}

impl Default for AssembleOptions {
    fn default() -> Self {
        Self {
            budget_tokens: DEFAULT_BUDGET_TOKENS,
            budget_bytes: DEFAULT_BUDGET_BYTES,
            detail_mode: DetailMode::Compact,
            compact_assembly: false,
            agent_snapshot: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ContextRef {
    pub kind: String,
    pub value: String,
    pub source: Option<String>,
    pub relevance: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ContextSection {
    pub id: Option<String>,
    pub title: String,
    pub body: String,
    pub refs: Vec<ContextRef>,
    pub relevance: Option<f64>,
    pub source_packet: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ToolInvocation {
    pub name: String,
    pub reducer: Option<String>,
    pub paths: Vec<String>,
    pub token_usage: Option<u64>,
    pub runtime_ms: Option<u64>,
    pub input: Value,
    pub output: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ReducerInvocation {
    pub name: String,
    pub token_usage: Option<u64>,
    pub runtime_ms: Option<u64>,
    pub output: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PacketFileRef {
    pub path: String,
    pub relevance: Option<f64>,
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PacketSymbolRef {
    pub name: String,
    pub file: Option<String>,
    pub kind: Option<String>,
    pub relevance: Option<f64>,
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct InputPacket {
    pub packet_id: Option<String>,
    pub summary: Option<String>,
    pub tool: Option<String>,
    pub tools: Vec<String>,
    pub reducer: Option<String>,
    pub reducers: Vec<String>,
    pub paths: Vec<String>,
    pub token_usage: Option<u64>,
    pub runtime_ms: Option<u64>,
    pub payload: Value,
    pub files: Vec<PacketFileRef>,
    pub symbols: Vec<PacketSymbolRef>,
    pub tool_invocations: Vec<ToolInvocation>,
    pub reducer_invocations: Vec<ReducerInvocation>,
    pub text_blobs: Vec<String>,
    pub sections: Vec<ContextSection>,
    pub refs: Vec<ContextRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AssemblySummary {
    pub input_packets: usize,
    pub sections_input: usize,
    pub sections_kept: usize,
    pub sections_dropped: usize,
    pub refs_input: usize,
    pub refs_kept: usize,
    pub refs_dropped: usize,
    pub budget_tokens: u64,
    pub budget_bytes: usize,
    pub estimated_tokens: u64,
    pub estimated_bytes: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AssembledPayload {
    pub sources: Vec<String>,
    pub sections: Vec<ContextSection>,
    pub refs: Vec<ContextRef>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AssembledPacket {
    pub schema_version: String,
    pub packet_id: Option<String>,
    pub tool: Option<String>,
    pub tools: Vec<String>,
    pub reducer: Option<String>,
    pub reducers: Vec<String>,
    pub paths: Vec<String>,
    pub token_usage: Option<u64>,
    pub runtime_ms: Option<u64>,
    pub payload: Value,
    pub tool_invocations: Vec<ToolInvocation>,
    pub reducer_invocations: Vec<ReducerInvocation>,
    pub text_blobs: Vec<String>,
    pub assembly: AssemblySummary,
}

pub fn assemble_packet_files(
    packet_paths: &[PathBuf],
    options: AssembleOptions,
) -> Result<AssembledPacket, CovyError> {
    let mut packets = Vec::with_capacity(packet_paths.len());
    for path in packet_paths {
        let value = read_json_file(path)?;
        let label = path.to_string_lossy().to_string();
        packets.push(InputPacket::from_value(value, &label));
    }

    Ok(assemble_packets(packets, options))
}

pub fn assemble_packets(
    mut packets: Vec<InputPacket>,
    options: AssembleOptions,
) -> AssembledPacket {
    let question_tokens = options
        .agent_snapshot
        .as_ref()
        .map(question_tokens)
        .unwrap_or_default();
    let mut tools = BTreeSet::new();
    let mut reducers = BTreeSet::new();
    let mut paths = BTreeSet::new();

    let mut all_sections = Vec::new();
    let mut total_runtime_ms = 0u64;

    let mut sources = Vec::new();
    let mut source_summaries = Vec::new();

    for (packet_idx, packet) in packets.iter_mut().enumerate() {
        let packet_label = packet
            .packet_id
            .clone()
            .filter(|id| !id.trim().is_empty())
            .unwrap_or_else(|| format!("packet-{}", packet_idx + 1));
        sources.push(packet_label.clone());

        if let Some(tool) = non_empty(packet.tool.as_deref()) {
            tools.insert(tool.to_string());
        }
        for tool in &packet.tools {
            if let Some(tool) = non_empty(Some(tool.as_str())) {
                tools.insert(tool.to_string());
            }
        }

        if let Some(reducer) = non_empty(packet.reducer.as_deref()) {
            reducers.insert(reducer.to_string());
        }
        for reducer in &packet.reducers {
            if let Some(reducer) = non_empty(Some(reducer.as_str())) {
                reducers.insert(reducer.to_string());
            }
        }

        for path in &packet.paths {
            if let Some(path) = non_empty(Some(path.as_str())) {
                paths.insert(normalize_path(path));
            }
        }
        for file in &packet.files {
            if let Some(path) = non_empty(Some(file.path.as_str())) {
                paths.insert(normalize_path(path));
            }
        }

        total_runtime_ms = total_runtime_ms.saturating_add(packet.runtime_ms.unwrap_or(0));

        let mut derived_refs = derive_refs(packet, &packet.payload, &packet_label);
        packet.refs.append(&mut derived_refs);
        packet.refs = dedupe_refs(packet.refs.drain(..));

        if packet.sections.is_empty() {
            packet.sections.push(build_default_section(
                packet,
                &packet_label,
                options.detail_mode,
            ));
        }

        for (section_idx, section) in packet.sections.iter_mut().enumerate() {
            if section.title.trim().is_empty() {
                section.title = format!("Section {}", section_idx + 1);
            }
            if section.source_packet.is_none() {
                section.source_packet = Some(packet_label.clone());
            }
            if section.refs.is_empty() {
                section.refs = packet.refs.clone();
            } else {
                section.refs = dedupe_refs(section.refs.clone().into_iter());
            }
            if section.relevance.is_none() {
                section.relevance = Some(infer_relevance(section));
            }

            if let Some(snapshot) = options.agent_snapshot.as_ref() {
                if should_compress_section(section, snapshot, &question_tokens) {
                    compress_section(section);
                }
            }

            let mut score = section.relevance.unwrap_or(0.5) + (section.refs.len() as f64 * 0.05);
            if let Some(snapshot) = options.agent_snapshot.as_ref() {
                score += section_focus_boost(section, snapshot);
                score += question_match_boost(section, &question_tokens);
            }
            all_sections.push((
                Reverse(F64Ord(score)),
                packet_idx,
                section_idx,
                section.clone(),
            ));
        }

        source_summaries.push(ToolInvocation {
            name: packet
                .tool
                .clone()
                .unwrap_or_else(|| "input-packet".to_string()),
            reducer: packet.reducer.clone(),
            paths: packet.paths.clone(),
            token_usage: packet.token_usage,
            runtime_ms: packet.runtime_ms,
            input: Value::Null,
            output: json!({
                "packet_id": packet_label,
                "sections": packet.sections.len(),
                "refs": packet.refs.len()
            }),
        });
    }

    let mut refs_input_keys = BTreeSet::new();
    for packet in &packets {
        for reference in &packet.refs {
            refs_input_keys.insert(ref_key(reference));
        }
        for section in &packet.sections {
            for reference in &section.refs {
                refs_input_keys.insert(ref_key(reference));
            }
        }
    }

    all_sections.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
    });

    let mut used_tokens = 0u64;
    let mut used_bytes = 0usize;
    let mut seen_ref_keys = BTreeSet::new();
    let mut selected_sections = Vec::new();

    for (_, _, _, mut section) in all_sections {
        section.refs.retain(|r| seen_ref_keys.insert(ref_key(r)));

        let section_tokens = estimate_section_tokens(&section);
        let section_bytes = estimate_json_bytes(&section);

        if exceeds_budget(used_tokens, section_tokens, options.budget_tokens)
            || exceeds_budget_usize(used_bytes, section_bytes, options.budget_bytes)
        {
            continue;
        }

        used_tokens = used_tokens.saturating_add(section_tokens);
        used_bytes = used_bytes.saturating_add(section_bytes);
        selected_sections.push(section);
    }

    let refs_input_count = refs_input_keys.len();

    let mut refs_by_key: HashMap<String, ContextRef> = HashMap::new();
    for section in &selected_sections {
        for reference in &section.refs {
            merge_ref(&mut refs_by_key, reference.clone());
        }
    }

    for packet in &packets {
        for reference in &packet.refs {
            merge_ref(&mut refs_by_key, reference.clone());
        }
    }

    let mut refs_ranked: Vec<_> = refs_by_key.into_values().collect();
    if let Some(snapshot) = options.agent_snapshot.as_ref() {
        for reference in &mut refs_ranked {
            let base = reference.relevance.unwrap_or(0.0);
            reference.relevance = Some(
                base + ref_focus_boost(reference, snapshot) + ref_question_boost(reference, &question_tokens),
            );
        }
    }
    refs_ranked.sort_by(|a, b| {
        b.relevance
            .unwrap_or(0.0)
            .total_cmp(&a.relevance.unwrap_or(0.0))
            .then_with(|| a.kind.cmp(&b.kind))
            .then_with(|| a.value.cmp(&b.value))
    });

    let mut selected_refs = Vec::new();
    for reference in refs_ranked {
        let ref_tokens = estimate_ref_tokens(&reference);
        let ref_bytes = estimate_json_bytes(&reference);
        if exceeds_budget(used_tokens, ref_tokens, options.budget_tokens)
            || exceeds_budget_usize(used_bytes, ref_bytes, options.budget_bytes)
        {
            continue;
        }

        used_tokens = used_tokens.saturating_add(ref_tokens);
        used_bytes = used_bytes.saturating_add(ref_bytes);
        selected_refs.push(reference);
    }

    let mut payload = AssembledPayload {
        sources: sources.clone(),
        sections: selected_sections,
        refs: selected_refs,
        truncated: false,
    };

    if options.compact_assembly {
        for section in &mut payload.sections {
            section.refs.clear();
        }
    }

    enforce_budget(&mut payload, &options);

    let payload_value = serde_json::to_value(&payload).unwrap_or(Value::Null);
    let payload_text = serde_json::to_string(&payload_value).unwrap_or_default();
    let final_tokens = estimate_tokens(&payload_text);
    let final_bytes = payload_text.len();

    let sections_input = packets.iter().map(|p| p.sections.len()).sum::<usize>();
    let refs_kept = payload.refs.len();
    let refs_dropped = refs_input_count.saturating_sub(refs_kept);
    let sections_kept = payload.sections.len();
    let sections_dropped = sections_input.saturating_sub(sections_kept);
    let truncated = sections_dropped > 0 || refs_dropped > 0 || payload.truncated;

    tools.insert("contextq".to_string());
    reducers.insert("assemble".to_string());

    AssembledPacket {
        schema_version: CONTEXTQ_SCHEMA_VERSION.to_string(),
        packet_id: Some("contextq-assembled-v1".to_string()),
        tool: Some("contextq".to_string()),
        tools: tools.into_iter().collect(),
        reducer: Some("assemble".to_string()),
        reducers: reducers.into_iter().collect(),
        paths: paths.into_iter().collect(),
        token_usage: Some(final_tokens),
        runtime_ms: Some(total_runtime_ms),
        payload: payload_value,
        tool_invocations: if options.compact_assembly || options.detail_mode == DetailMode::Compact {
            Vec::new()
        } else {
            source_summaries
        },
        reducer_invocations: if options.compact_assembly || options.detail_mode == DetailMode::Compact {
            Vec::new()
        } else {
            vec![ReducerInvocation {
                name: "contextq.assemble".to_string(),
                token_usage: Some(final_tokens),
                runtime_ms: None,
                output: json!({
                    "sections_kept": sections_kept,
                    "refs_kept": refs_kept,
                    "truncated": truncated
                }),
            }]
        },
        text_blobs: if options.compact_assembly || options.detail_mode == DetailMode::Compact {
            Vec::new()
        } else {
            payload
                .sections
                .iter()
                .map(|section| section.body.clone())
                .collect()
        },
        assembly: AssemblySummary {
            input_packets: packets.len(),
            sections_input,
            sections_kept,
            sections_dropped,
            refs_input: refs_input_count,
            refs_kept,
            refs_dropped,
            budget_tokens: options.budget_tokens,
            budget_bytes: options.budget_bytes,
            estimated_tokens: final_tokens,
            estimated_bytes: final_bytes,
            truncated,
        },
    }
}

impl InputPacket {
    pub fn from_value(value: Value, source_label: &str) -> Self {
        let mut parsed = serde_json::from_value::<InputPacket>(value.clone()).unwrap_or_default();

        if parsed.packet_id.is_none() {
            parsed.packet_id = Some(source_label.to_string());
        }

        if parsed.payload.is_null() {
            parsed.payload = value;
        }

        parsed
    }
}

fn build_default_section(
    packet: &InputPacket,
    packet_label: &str,
    detail_mode: DetailMode,
) -> ContextSection {
    let title = if let Some(reducer) = non_empty(packet.reducer.as_deref()) {
        format!("{reducer} output")
    } else if let Some(tool) = non_empty(packet.tool.as_deref()) {
        format!("{tool} output")
    } else {
        format!("packet {packet_label}")
    };

    let body = match &packet.payload {
        Value::Null => packet
            .summary
            .clone()
            .unwrap_or_else(|| "{}".to_string()),
        Value::String(text) => match detail_mode {
            DetailMode::Rich => text.clone(),
            DetailMode::Compact => packet
                .summary
                .clone()
                .unwrap_or_else(|| truncate_text(text, 180)),
        },
        other => match detail_mode {
            DetailMode::Rich => {
                serde_json::to_string_pretty(other).unwrap_or_else(|_| "{}".to_string())
            }
            DetailMode::Compact => packet
                .summary
                .clone()
                .unwrap_or_else(|| summarize_payload(other)),
        },
    };

    ContextSection {
        id: None,
        title,
        body,
        refs: packet.refs.clone(),
        relevance: None,
        source_packet: Some(packet_label.to_string()),
    }
}

fn summarize_payload(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(v) => v.to_string(),
        Value::Number(v) => v.to_string(),
        Value::String(text) => truncate_text(text, 180),
        Value::Array(items) => format!("array(len={})", items.len()),
        Value::Object(map) => summarize_payload_object(map),
    }
}

fn summarize_payload_object(map: &serde_json::Map<String, Value>) -> String {
    if let Some(Value::Object(gate)) = map.get("gate_result") {
        let passed = gate
            .get("passed")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let total = gate
            .get("total_coverage_pct")
            .and_then(Value::as_f64)
            .map(|value| format!("{value:.1}%"))
            .unwrap_or_else(|| "n/a".to_string());
        let changed = gate
            .get("changed_coverage_pct")
            .and_then(Value::as_f64)
            .map(|value| format!("{value:.1}%"))
            .unwrap_or_else(|| "n/a".to_string());
        return format!("gate_result: passed={passed} total={total} changed={changed}");
    }

    if let Some(Value::Object(result)) = map.get("result") {
        let selected = result
            .get("selected_tests")
            .and_then(Value::as_array)
            .map(|items| items.len())
            .unwrap_or(0);
        let smoke = result
            .get("smoke_tests")
            .and_then(Value::as_array)
            .map(|items| items.len())
            .unwrap_or(0);
        let missing = result
            .get("missing_mappings")
            .and_then(Value::as_array)
            .map(|items| items.len())
            .unwrap_or(0);
        let confidence = result
            .get("confidence")
            .and_then(Value::as_f64)
            .map(|value| format!("{value:.2}"))
            .unwrap_or_else(|| "n/a".to_string());
        return format!(
            "impact_result: selected={selected} smoke={smoke} missing={missing} confidence={confidence}"
        );
    }

    if map.contains_key("files_ranked") && map.contains_key("symbols_ranked") {
        let files = map
            .get("files_ranked")
            .and_then(Value::as_array)
            .map(|items| items.len())
            .unwrap_or(0);
        let symbols = map
            .get("symbols_ranked")
            .and_then(Value::as_array)
            .map(|items| items.len())
            .unwrap_or(0);
        let edges = map
            .get("edges")
            .and_then(Value::as_array)
            .map(|items| items.len())
            .unwrap_or(0);
        return format!("repo_map: files={files} symbols={symbols} edges={edges}");
    }

    if map.contains_key("total_failures") && map.contains_key("unique_failures") {
        let total = map
            .get("total_failures")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let unique = map
            .get("unique_failures")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let duplicates = map
            .get("duplicates_removed")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        return format!(
            "stack_failures: total={total} unique={unique} duplicates_removed={duplicates}"
        );
    }

    if map.contains_key("total_diagnostics") && map.contains_key("unique_diagnostics") {
        let total = map
            .get("total_diagnostics")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let unique = map
            .get("unique_diagnostics")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let duplicates = map
            .get("duplicates_removed")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        return format!(
            "build_diagnostics: total={total} unique={unique} duplicates_removed={duplicates}"
        );
    }

    let mut keys = map.keys().cloned().collect::<Vec<_>>();
    keys.sort();
    if keys.len() > 6 {
        keys.truncate(6);
    }
    format!(
        "object(keys={}): {}",
        map.len(),
        keys.join(", ")
    )
}

fn truncate_text(input: &str, cap: usize) -> String {
    let normalized = input.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= cap {
        return normalized;
    }

    let mut truncated = String::new();
    for ch in normalized.chars().take(cap.saturating_sub(3)) {
        truncated.push(ch);
    }
    truncated.push_str("...");
    truncated
}

fn derive_refs(packet: &InputPacket, payload: &Value, packet_label: &str) -> Vec<ContextRef> {
    let mut refs = Vec::new();

    for file in &packet.files {
        if let Some(path) = non_empty(Some(file.path.as_str())) {
            refs.push(ContextRef {
                kind: "file".to_string(),
                value: normalize_path(path),
                source: Some(packet_label.to_string()),
                relevance: file.relevance.or(Some(0.8)),
            });
        }
    }

    for symbol in &packet.symbols {
        if let Some(name) = non_empty(Some(symbol.name.as_str())) {
            refs.push(ContextRef {
                kind: "symbol".to_string(),
                value: name.to_string(),
                source: Some(packet_label.to_string()),
                relevance: symbol.relevance.or(Some(0.7)),
            });
        }
        if let Some(file) = symbol.file.as_deref().and_then(|v| non_empty(Some(v))) {
            refs.push(ContextRef {
                kind: "file".to_string(),
                value: normalize_path(file),
                source: Some(packet_label.to_string()),
                relevance: symbol.relevance.or(Some(0.65)),
            });
        }
    }

    for path in &packet.paths {
        if let Some(path) = non_empty(Some(path.as_str())) {
            refs.push(ContextRef {
                kind: "file".to_string(),
                value: normalize_path(path),
                source: Some(packet_label.to_string()),
                relevance: Some(0.8),
            });
        }
    }

    if let Value::Object(map) = payload {
        for key in ["paths", "files"] {
            if let Some(Value::Array(items)) = map.get(key) {
                for item in items {
                    if let Some(path) = item.as_str() {
                        refs.push(ContextRef {
                            kind: "file".to_string(),
                            value: normalize_path(path),
                            source: Some(packet_label.to_string()),
                            relevance: Some(0.7),
                        });
                    }
                }
            }
        }

        for key in ["selected_tests", "smoke_tests", "missing_mappings"] {
            if let Some(Value::Array(items)) = map.get(key) {
                for item in items {
                    if let Some(symbol) = item.as_str() {
                        refs.push(ContextRef {
                            kind: "symbol".to_string(),
                            value: symbol.to_string(),
                            source: Some(packet_label.to_string()),
                            relevance: Some(0.6),
                        });
                    }
                }
            }
        }

        if let Some(Value::Array(tests)) = map.get("tests") {
            for item in tests {
                if let Value::Object(test) = item {
                    for key in ["id", "name"] {
                        if let Some(Value::String(symbol)) = test.get(key) {
                            refs.push(ContextRef {
                                kind: "symbol".to_string(),
                                value: symbol.clone(),
                                source: Some(packet_label.to_string()),
                                relevance: Some(0.55),
                            });
                        }
                    }
                }
            }
        }
    }

    refs
}

fn infer_relevance(section: &ContextSection) -> f64 {
    let mut score = 0.5;
    let corpus = format!("{} {}", section.title, section.body).to_ascii_lowercase();

    if contains_any(
        &corpus,
        &["critical", "fail", "failed", "error", "regression", "panic"],
    ) {
        score += 0.7;
    }

    if contains_any(&corpus, &["warning", "uncovered", "missing", "stale"]) {
        score += 0.3;
    }

    if contains_any(&corpus, &["passed", "ok", "green"]) {
        score += 0.1;
    }

    score + (section.refs.len() as f64 * 0.04)
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

fn question_tokens(snapshot: &suite_packet_core::AgentSnapshotPayload) -> BTreeSet<String> {
    snapshot
        .open_questions
        .iter()
        .flat_map(|question| tokenize_text(&question.text))
        .collect()
}

fn tokenize_text(input: &str) -> Vec<String> {
    input
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != ':' && c != '/')
        .map(|part| part.trim().to_ascii_lowercase())
        .filter(|part| part.len() >= 2)
        .collect()
}

fn section_focus_boost(
    section: &ContextSection,
    snapshot: &suite_packet_core::AgentSnapshotPayload,
) -> f64 {
    let mut boost = 0.0;
    let file_refs = section_file_refs(section);
    let symbol_refs = section_symbol_refs(section);

    if file_refs
        .iter()
        .any(|path| path_matches_any(path, &snapshot.focus_paths))
    {
        boost += 0.35;
    }
    if symbol_refs
        .iter()
        .any(|symbol| symbol_matches_any(symbol, &snapshot.focus_symbols))
    {
        boost += 0.25;
    }

    boost
}

fn question_match_boost(section: &ContextSection, question_tokens: &BTreeSet<String>) -> f64 {
    if question_tokens.is_empty() {
        return 0.0;
    }

    let corpus = section_corpus(section);
    let matched = question_tokens
        .iter()
        .filter(|token| corpus.contains(token.as_str()))
        .count();
    if matched == 0 {
        0.0
    } else {
        ((matched as f64 / question_tokens.len() as f64) * 0.25).min(0.25)
    }
}

fn ref_focus_boost(
    reference: &ContextRef,
    snapshot: &suite_packet_core::AgentSnapshotPayload,
) -> f64 {
    let normalized = normalize_path(reference.value.trim());
    match reference.kind.as_str() {
        "file" | "path" => {
            if path_matches_any(&normalized, &snapshot.focus_paths) {
                0.3
            } else {
                0.0
            }
        }
        "symbol" => {
            if symbol_matches_any(reference.value.trim(), &snapshot.focus_symbols) {
                0.2
            } else {
                0.0
            }
        }
        _ => 0.0,
    }
}

fn ref_question_boost(reference: &ContextRef, question_tokens: &BTreeSet<String>) -> f64 {
    if question_tokens.is_empty() {
        return 0.0;
    }

    let haystack = reference.value.to_ascii_lowercase();
    let matched = question_tokens
        .iter()
        .filter(|token| haystack.contains(token.as_str()))
        .count();
    if matched == 0 {
        0.0
    } else {
        ((matched as f64 / question_tokens.len() as f64) * 0.15).min(0.15)
    }
}

fn should_compress_section(
    section: &ContextSection,
    snapshot: &suite_packet_core::AgentSnapshotPayload,
    question_tokens: &BTreeSet<String>,
) -> bool {
    if snapshot.files_read.is_empty() {
        return false;
    }

    let file_refs = section_file_refs(section);
    if file_refs.is_empty() {
        return false;
    }
    if !file_refs
        .iter()
        .all(|path| path_matches_any(path, &snapshot.files_read))
    {
        return false;
    }
    if file_refs
        .iter()
        .any(|path| path_matches_any(path, &snapshot.files_edited))
    {
        return false;
    }
    if file_refs
        .iter()
        .any(|path| path_matches_any(path, &snapshot.focus_paths))
    {
        return false;
    }
    if section_symbol_refs(section)
        .iter()
        .any(|symbol| symbol_matches_any(symbol, &snapshot.focus_symbols))
    {
        return false;
    }
    if question_match_boost(section, question_tokens) > 0.0 {
        return false;
    }

    true
}

fn compress_section(section: &mut ContextSection) {
    let files = section_file_refs(section);
    let label = if files.is_empty() {
        section
            .source_packet
            .clone()
            .unwrap_or_else(|| "section".to_string())
    } else {
        files.into_iter().take(2).collect::<Vec<_>>().join(", ")
    };
    section.body = format!("Reminder: already reviewed {label}");
    section.relevance = Some(section.relevance.unwrap_or(0.5) * 0.6);
}

fn section_file_refs(section: &ContextSection) -> Vec<String> {
    section
        .refs
        .iter()
        .filter(|reference| matches!(reference.kind.as_str(), "file" | "path"))
        .map(|reference| normalize_path(reference.value.trim()))
        .collect()
}

fn section_symbol_refs(section: &ContextSection) -> Vec<String> {
    section
        .refs
        .iter()
        .filter(|reference| reference.kind == "symbol")
        .map(|reference| reference.value.trim().to_ascii_lowercase())
        .collect()
}

fn section_corpus(section: &ContextSection) -> String {
    let mut corpus = format!("{} {}", section.title, section.body).to_ascii_lowercase();
    for reference in &section.refs {
        corpus.push(' ');
        corpus.push_str(&reference.value.to_ascii_lowercase());
    }
    corpus
}

fn path_matches_any(path: &str, candidates: &[String]) -> bool {
    let normalized = normalize_path(path);
    candidates.iter().any(|candidate| {
        let candidate = normalize_path(candidate);
        normalized == candidate
            || normalized.starts_with(&candidate)
            || candidate.starts_with(&normalized)
    })
}

fn symbol_matches_any(symbol: &str, candidates: &[String]) -> bool {
    let normalized = symbol.to_ascii_lowercase();
    candidates.iter().any(|candidate| {
        let candidate = candidate.to_ascii_lowercase();
        normalized == candidate || normalized.contains(&candidate) || candidate.contains(&normalized)
    })
}

fn non_empty(input: Option<&str>) -> Option<&str> {
    let value = input?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn normalize_path(path: &str) -> String {
    path.replace('\\', "/")
}

fn ref_key(reference: &ContextRef) -> String {
    let kind = if reference.kind.trim().is_empty() {
        "file".to_string()
    } else {
        reference.kind.trim().to_ascii_lowercase()
    };

    let raw_value = reference.value.trim();
    let value = match kind.as_str() {
        "file" | "path" => normalize_path(raw_value).to_ascii_lowercase(),
        _ => raw_value.to_ascii_lowercase(),
    };

    format!("{kind}::{value}")
}

fn dedupe_refs(refs: impl IntoIterator<Item = ContextRef>) -> Vec<ContextRef> {
    let mut deduped = HashMap::new();
    for reference in refs {
        merge_ref(&mut deduped, reference);
    }

    let mut refs: Vec<_> = deduped.into_values().collect();
    refs.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.value.cmp(&b.value)));
    refs
}

fn merge_ref(map: &mut HashMap<String, ContextRef>, mut incoming: ContextRef) {
    if incoming.kind.trim().is_empty() {
        incoming.kind = "file".to_string();
    }

    let key = ref_key(&incoming);
    match map.get_mut(&key) {
        Some(existing) => {
            let best_relevance = existing
                .relevance
                .unwrap_or(0.0)
                .max(incoming.relevance.unwrap_or(0.0));
            existing.relevance = Some(best_relevance);
            if existing.source.is_none() {
                existing.source = incoming.source.take();
            }
        }
        None => {
            map.insert(key, incoming);
        }
    }
}

fn estimate_tokens(text: &str) -> u64 {
    ((text.chars().count() as u64).saturating_add(3)) / 4
}

fn estimate_json_bytes<T: Serialize>(value: &T) -> usize {
    serde_json::to_vec(value).map(|buf| buf.len()).unwrap_or(0)
}

fn estimate_section_tokens(section: &ContextSection) -> u64 {
    let mut text = String::with_capacity(section.title.len() + section.body.len() + 32);
    text.push_str(&section.title);
    text.push('\n');
    text.push_str(&section.body);
    for reference in &section.refs {
        text.push('\n');
        text.push_str(&reference.kind);
        text.push(':');
        text.push_str(&reference.value);
    }
    estimate_tokens(&text)
}

fn estimate_ref_tokens(reference: &ContextRef) -> u64 {
    estimate_tokens(&format!("{}:{}", reference.kind, reference.value))
}

fn exceeds_budget(used: u64, add: u64, cap: u64) -> bool {
    used.saturating_add(add) > cap
}

fn exceeds_budget_usize(used: usize, add: usize, cap: usize) -> bool {
    used.saturating_add(add) > cap
}

fn enforce_budget(payload: &mut AssembledPayload, options: &AssembleOptions) {
    while let Ok(serialized) = serde_json::to_string(payload) {
        let token_estimate = estimate_tokens(&serialized);
        let byte_estimate = serialized.len();

        if token_estimate <= options.budget_tokens && byte_estimate <= options.budget_bytes {
            break;
        }

        payload.truncated = true;

        if !payload.refs.is_empty() {
            payload.refs.pop();
            continue;
        }

        if let Some(last) = payload.sections.last_mut() {
            if last.body.len() > 64 {
                let new_len = (last.body.len() * 3) / 4;
                last.body.truncate(new_len.max(32));
                last.body.push_str(" ...");
                continue;
            }
        }

        if !payload.sections.is_empty() {
            payload.sections.pop();
            continue;
        }
    }
}

fn read_json_file(path: &Path) -> Result<Value, CovyError> {
    let content = std::fs::read_to_string(path).map_err(|source| CovyError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    serde_json::from_str::<Value>(&content).map_err(|source| CovyError::Parse {
        format: "packet-json".to_string(),
        detail: source.to_string(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct F64Ord(f64);

impl Eq for F64Ord {}

impl PartialOrd for F64Ord {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for F64Ord {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn assemble_dedupes_refs_and_keeps_higher_ranked_sections() {
        let packet_a = InputPacket {
            packet_id: Some("diffy".to_string()),
            sections: vec![ContextSection {
                title: "Critical failure in src/lib.rs".to_string(),
                body: "error on uncovered lines".to_string(),
                refs: vec![ContextRef {
                    kind: "file".to_string(),
                    value: "src/lib.rs".to_string(),
                    source: Some("diffy".to_string()),
                    relevance: Some(0.9),
                }],
                relevance: Some(1.2),
                ..ContextSection::default()
            }],
            ..InputPacket::default()
        };

        let packet_b = InputPacket {
            packet_id: Some("impact".to_string()),
            sections: vec![ContextSection {
                title: "Impacted tests".to_string(),
                body: "selected tests list".to_string(),
                refs: vec![ContextRef {
                    kind: "file".to_string(),
                    value: "src/lib.rs".to_string(),
                    source: Some("impact".to_string()),
                    relevance: Some(0.7),
                }],
                relevance: Some(0.6),
                ..ContextSection::default()
            }],
            ..InputPacket::default()
        };

        let assembled = assemble_packets(
            vec![packet_a, packet_b],
            AssembleOptions {
                budget_tokens: 1000,
                budget_bytes: 50_000,
                ..AssembleOptions::default()
            },
        );

        assert_eq!(assembled.schema_version, CONTEXTQ_SCHEMA_VERSION);
        assert_eq!(assembled.assembly.input_packets, 2);
        let payload: AssembledPayload = serde_json::from_value(assembled.payload).unwrap();
        assert_eq!(payload.refs.len(), 1);
        assert_eq!(payload.sections.len(), 2);
    }

    #[test]
    fn assemble_respects_tight_budget() {
        let long = "x".repeat(4000);
        let packet = InputPacket {
            packet_id: Some("diffy".to_string()),
            sections: vec![ContextSection {
                title: "Very large section".to_string(),
                body: long,
                relevance: Some(1.0),
                ..ContextSection::default()
            }],
            ..InputPacket::default()
        };

        let assembled = assemble_packets(
            vec![packet],
            AssembleOptions {
                budget_tokens: 60,
                budget_bytes: 500,
                ..AssembleOptions::default()
            },
        );

        assert_eq!(assembled.schema_version, CONTEXTQ_SCHEMA_VERSION);
        assert!(assembled.assembly.truncated);
        assert!(assembled.token_usage.unwrap_or(0) <= 60);
        assert!(assembled.assembly.estimated_bytes <= 500);
    }

    #[test]
    fn assemble_packet_files_reads_inputs() {
        let dir = TempDir::new().unwrap();
        let a = dir.path().join("a.json");
        let b = dir.path().join("b.json");

        std::fs::write(
            &a,
            r#"{"packet_id":"a","paths":["src/a.rs"],"payload":{"selected_tests":["foo::bar"]}}"#,
        )
        .unwrap();
        std::fs::write(
            &b,
            r#"{"packet_id":"b","payload":{"paths":["src/a.rs","src/b.rs"]}}"#,
        )
        .unwrap();

        let assembled = assemble_packet_files(
            &[a, b],
            AssembleOptions {
                budget_tokens: 1200,
                budget_bytes: 24_000,
                ..AssembleOptions::default()
            },
        )
        .unwrap();

        assert_eq!(assembled.assembly.input_packets, 2);
        assert!(assembled.paths.contains(&"src/a.rs".to_string()));
    }

    #[test]
    fn derives_refs_from_envelope_top_level_refs() {
        let packet = InputPacket {
            packet_id: Some("mapy".to_string()),
            files: vec![PacketFileRef {
                path: "src/main.rs".to_string(),
                relevance: Some(0.9),
                source: Some("mapy.repo".to_string()),
            }],
            symbols: vec![PacketSymbolRef {
                name: "run".to_string(),
                file: Some("src/main.rs".to_string()),
                kind: Some("function".to_string()),
                relevance: Some(0.8),
                source: Some("mapy.repo".to_string()),
            }],
            ..InputPacket::default()
        };

        let assembled = assemble_packets(
            vec![packet],
            AssembleOptions {
                budget_tokens: 1200,
                budget_bytes: 24_000,
                ..AssembleOptions::default()
            },
        );
        let payload: AssembledPayload = serde_json::from_value(assembled.payload).unwrap();

        assert!(payload
            .refs
            .iter()
            .any(|r| r.kind == "file" && r.value == "src/main.rs"));
        assert!(payload
            .refs
            .iter()
            .any(|r| r.kind == "symbol" && r.value == "run"));
    }

    #[test]
    fn default_section_body_uses_pretty_json_in_rich_mode() {
        let packet = InputPacket {
            packet_id: Some("mapy".to_string()),
            payload: json!({
                "alpha": "beta",
                "items": [1, 2],
            }),
            ..InputPacket::default()
        };

        let assembled = assemble_packets(
            vec![packet],
            AssembleOptions {
                detail_mode: DetailMode::Rich,
                ..AssembleOptions::default()
            },
        );
        let payload: AssembledPayload = serde_json::from_value(assembled.payload).unwrap();
        let body = &payload.sections[0].body;
        assert!(body.contains('\n'));
        assert!(body.contains("  \"alpha\""));
    }

    #[test]
    fn default_section_body_prefers_packet_summary_in_compact_mode() {
        let packet = InputPacket {
            packet_id: Some("mapy".to_string()),
            summary: Some("repo_map files=4 symbols=24 edges=0".to_string()),
            payload: json!({
                "files_ranked": [{"file_idx": 0, "score": 0.9}],
                "symbols_ranked": [{"symbol_idx": 0, "score": 0.8}],
                "edges": []
            }),
            ..InputPacket::default()
        };

        let assembled = assemble_packets(
            vec![packet],
            AssembleOptions {
                detail_mode: DetailMode::Compact,
                ..AssembleOptions::default()
            },
        );
        let payload: AssembledPayload = serde_json::from_value(assembled.payload).unwrap();
        assert_eq!(payload.sections[0].body, "repo_map files=4 symbols=24 edges=0");
    }

    #[test]
    fn compact_assembly_drops_duplicate_section_refs_and_text_blobs() {
        let packet = InputPacket {
            packet_id: Some("diffy".to_string()),
            sections: vec![ContextSection {
                title: "Diff".to_string(),
                body: "critical regression".to_string(),
                refs: vec![ContextRef {
                    kind: "file".to_string(),
                    value: "src/lib.rs".to_string(),
                    source: Some("diffy".to_string()),
                    relevance: Some(0.9),
                }],
                ..ContextSection::default()
            }],
            ..InputPacket::default()
        };

        let assembled = assemble_packets(
            vec![packet],
            AssembleOptions {
                compact_assembly: true,
                ..AssembleOptions::default()
            },
        );
        let payload: AssembledPayload = serde_json::from_value(assembled.payload).unwrap();
        assert_eq!(assembled.text_blobs.len(), 0);
        assert!(assembled.tool_invocations.is_empty());
        assert!(assembled.reducer_invocations.is_empty());
        assert_eq!(payload.sections.len(), 1);
        assert!(payload.sections[0].refs.is_empty());
        assert_eq!(payload.refs.len(), 1);
    }

    #[test]
    fn task_aware_assembly_boosts_focus_and_compresses_read_sections() {
        let already_read = InputPacket {
            packet_id: Some("diffy".to_string()),
            sections: vec![ContextSection {
                title: "Diff".to_string(),
                body: "StopWatch.java changed on lines 10-20".to_string(),
                refs: vec![ContextRef {
                    kind: "file".to_string(),
                    value: "src/time/StopWatch.java".to_string(),
                    source: Some("diffy.analyze".to_string()),
                    relevance: Some(0.9),
                }],
                relevance: Some(0.9),
                ..ContextSection::default()
            }],
            ..InputPacket::default()
        };
        let focused = InputPacket {
            packet_id: Some("mapy".to_string()),
            sections: vec![ContextSection {
                title: "Neighbors".to_string(),
                body: "DateUtils references split() in the time package".to_string(),
                refs: vec![
                    ContextRef {
                        kind: "file".to_string(),
                        value: "src/time/DateUtils.java".to_string(),
                        source: Some("mapy.repo".to_string()),
                        relevance: Some(0.7),
                    },
                    ContextRef {
                        kind: "symbol".to_string(),
                        value: "split".to_string(),
                        source: Some("mapy.repo".to_string()),
                        relevance: Some(0.7),
                    },
                ],
                relevance: Some(0.7),
                ..ContextSection::default()
            }],
            ..InputPacket::default()
        };

        let assembled = assemble_packets(
            vec![already_read, focused],
            AssembleOptions {
                budget_tokens: 1200,
                budget_bytes: 24_000,
                agent_snapshot: Some(suite_packet_core::AgentSnapshotPayload {
                    task_id: "task-a".to_string(),
                    focus_paths: vec!["src/time/DateUtils.java".to_string()],
                    focus_symbols: vec!["split".to_string()],
                    files_read: vec!["src/time/StopWatch.java".to_string()],
                    files_edited: Vec::new(),
                    active_decisions: Vec::new(),
                    completed_steps: vec!["read_diff".to_string()],
                    open_questions: vec![suite_packet_core::AgentQuestion {
                        id: "q1".to_string(),
                        text: "Does DateUtils call split()?".to_string(),
                    }],
                    event_count: 3,
                    last_event_at_unix: Some(3),
                }),
                ..AssembleOptions::default()
            },
        );
        let payload: AssembledPayload = serde_json::from_value(assembled.payload).unwrap();

        assert_eq!(payload.sections[0].title, "Neighbors");
        assert!(payload
            .sections
            .iter()
            .any(|section| section.body.starts_with("Reminder: already reviewed")));
    }
}
