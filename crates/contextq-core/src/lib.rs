pub mod error {
    pub use suite_packet_core::error::*;
}

use std::cmp::Reverse;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use suite_foundation_core::error::CovyError;

pub const DEFAULT_BUDGET_TOKENS: u64 = 1200;
pub const DEFAULT_BUDGET_BYTES: usize = 24_000;

#[derive(Debug, Clone, Copy)]
pub struct AssembleOptions {
    pub budget_tokens: u64,
    pub budget_bytes: usize,
}

impl Default for AssembleOptions {
    fn default() -> Self {
        Self {
            budget_tokens: DEFAULT_BUDGET_TOKENS,
            budget_bytes: DEFAULT_BUDGET_BYTES,
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
pub struct InputPacket {
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

        total_runtime_ms = total_runtime_ms.saturating_add(packet.runtime_ms.unwrap_or(0));

        let mut derived_refs = derive_refs(packet, &packet.payload, &packet_label);
        packet.refs.append(&mut derived_refs);
        packet.refs = dedupe_refs(packet.refs.drain(..));

        if packet.sections.is_empty() {
            packet
                .sections
                .push(build_default_section(packet, &packet_label));
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

            let score = section.relevance.unwrap_or(0.5) + (section.refs.len() as f64 * 0.05);
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

    enforce_budget(&mut payload, options);

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
        packet_id: Some("contextq-assembled-v1".to_string()),
        tool: Some("contextq".to_string()),
        tools: tools.into_iter().collect(),
        reducer: Some("assemble".to_string()),
        reducers: reducers.into_iter().collect(),
        paths: paths.into_iter().collect(),
        token_usage: Some(final_tokens),
        runtime_ms: Some(total_runtime_ms),
        payload: payload_value,
        tool_invocations: source_summaries,
        reducer_invocations: vec![ReducerInvocation {
            name: "contextq.assemble".to_string(),
            token_usage: Some(final_tokens),
            runtime_ms: None,
            output: json!({
                "sections_kept": sections_kept,
                "refs_kept": refs_kept,
                "truncated": truncated
            }),
        }],
        text_blobs: payload
            .sections
            .iter()
            .map(|section| section.body.clone())
            .collect(),
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

fn build_default_section(packet: &InputPacket, packet_label: &str) -> ContextSection {
    let title = if let Some(reducer) = non_empty(packet.reducer.as_deref()) {
        format!("{reducer} output")
    } else if let Some(tool) = non_empty(packet.tool.as_deref()) {
        format!("{tool} output")
    } else {
        format!("packet {packet_label}")
    };

    let body = match &packet.payload {
        Value::Null => "{}".to_string(),
        Value::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| "{}".to_string()),
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

fn derive_refs(packet: &InputPacket, payload: &Value, packet_label: &str) -> Vec<ContextRef> {
    let mut refs = Vec::new();

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

fn enforce_budget(payload: &mut AssembledPayload, options: AssembleOptions) {
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
            },
        );

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
            },
        );

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
            },
        )
        .unwrap();

        assert_eq!(assembled.assembly.input_packets, 2);
        assert!(assembled.paths.contains(&"src/a.rs".to_string()));
    }
}
