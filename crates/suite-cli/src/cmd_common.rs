use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::ValueEnum;
use serde_json::{json, Value};
use suite_packet_core::{CoverageData, CoverageFormat, EnvelopeV1, JsonProfile, PacketWrapperV1};

pub fn parse_daemon_env_flag(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| {
        !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no" | "off"
        )
    })
}

pub fn via_daemon_env_enabled() -> bool {
    parse_daemon_env_flag(std::env::var("PACKET28_VIA_DAEMON").ok().as_deref())
}

pub fn resolve_report_format(explicit: Option<&str>) -> String {
    match explicit {
        Some(fmt) => fmt.to_string(),
        None => "terminal".to_string(),
    }
}

#[derive(Debug, Clone, Copy, ValueEnum, Default, PartialEq, Eq)]
pub enum JsonProfileArg {
    #[default]
    Compact,
    Full,
    Handle,
}

impl From<JsonProfileArg> for JsonProfile {
    fn from(value: JsonProfileArg) -> Self {
        match value {
            JsonProfileArg::Compact => JsonProfile::Compact,
            JsonProfileArg::Full => JsonProfile::Full,
            JsonProfileArg::Handle => JsonProfile::Handle,
        }
    }
}

pub fn resolve_machine_profile(
    json_profile: Option<JsonProfileArg>,
    legacy_format: Option<&str>,
    legacy_flag_name: &str,
) -> Result<Option<JsonProfile>> {
    if let Some(profile) = json_profile {
        if let Some(fmt) = legacy_format {
            if !fmt.eq_ignore_ascii_case("json") {
                anyhow::bail!(
                    "Conflicting output flags: --json and {} {}",
                    legacy_flag_name,
                    fmt
                );
            }
        }
        return Ok(Some(profile.into()));
    }

    if legacy_format.is_some_and(|fmt| fmt.eq_ignore_ascii_case("json")) {
        return Ok(Some(JsonProfile::Compact));
    }

    Ok(None)
}

pub fn emit_machine_envelope<T: serde::Serialize + Clone>(
    packet_type: &str,
    envelope: &EnvelopeV1<T>,
    profile: JsonProfile,
    pretty: bool,
    artifact_root: &Path,
    debug: Option<Value>,
) -> Result<()> {
    let mut packet = serde_json::to_value(envelope)?;

    match profile {
        JsonProfile::Full => {
            if let Some(ref debug) = debug {
                insert_payload_debug(&mut packet, debug.clone());
            }
        }
        JsonProfile::Compact => {
            compact_packet_payload(packet_type, &mut packet);
        }
        JsonProfile::Handle => {
            let handle =
                suite_packet_core::write_packet_artifact(artifact_root, packet_type, envelope)
                    .map_err(|source| anyhow::anyhow!(source.to_string()))?;
            compact_packet_payload(packet_type, &mut packet);
            attach_artifact_handle(&mut packet, serde_json::to_value(handle)?);
        }
    }

    refresh_packet_budget(&mut packet);

    let mut wrapper = PacketWrapperV1::new(packet_type.to_string(), packet);
    wrapper.cache_hit = debug.as_ref().and_then(extract_cache_hit).unwrap_or(false);
    emit_json(&serde_json::to_value(wrapper)?, pretty)
}

pub fn emit_machine_wrapper<T: serde::Serialize + Clone>(
    wrapper: &PacketWrapperV1<EnvelopeV1<T>>,
    profile: JsonProfile,
    pretty: bool,
    artifact_root: &Path,
    debug: Option<Value>,
) -> Result<()> {
    let mut packet = serde_json::to_value(&wrapper.packet)?;

    match profile {
        JsonProfile::Full => {
            if let Some(ref debug) = debug {
                insert_payload_debug(&mut packet, debug.clone());
            }
        }
        JsonProfile::Compact => {
            compact_packet_payload(&wrapper.packet_type, &mut packet);
        }
        JsonProfile::Handle => {
            let handle = suite_packet_core::write_packet_artifact(
                artifact_root,
                &wrapper.packet_type,
                &wrapper.packet,
            )
            .map_err(|source| anyhow::anyhow!(source.to_string()))?;
            compact_packet_payload(&wrapper.packet_type, &mut packet);
            attach_artifact_handle(&mut packet, serde_json::to_value(handle)?);
        }
    }

    refresh_packet_budget(&mut packet);

    let mut output = PacketWrapperV1::new(wrapper.packet_type.clone(), packet);
    output.schema_version = wrapper.schema_version.clone();
    output.cache_hit =
        wrapper.cache_hit || debug.as_ref().and_then(extract_cache_hit).unwrap_or(false);
    emit_json(&serde_json::to_value(output)?, pretty)
}

pub fn emit_json(value: &Value, pretty: bool) -> Result<()> {
    if pretty {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        println!("{}", serde_json::to_string(value)?);
    }
    Ok(())
}

pub fn emit_machine_error(
    command: &str,
    error: &anyhow::Error,
    pretty: bool,
    target: Option<&str>,
    retry_hint: Option<Value>,
) -> Result<()> {
    let causes = error
        .chain()
        .skip(1)
        .map(|cause| Value::String(cause.to_string()))
        .collect::<Vec<_>>();
    emit_json(
        &json!({
            "schema_version": "suite.error.v1",
            "command": command,
            "message": error.to_string(),
            "target": target,
            "retry_hint": retry_hint,
            "causes": causes,
        }),
        pretty,
    )
}

pub fn resolve_artifact_root(explicit_root: Option<&Path>) -> PathBuf {
    if let Some(root) = explicit_root {
        return root.to_path_buf();
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

pub fn caller_cwd() -> Result<PathBuf> {
    std::env::current_dir().context("failed to resolve current directory")
}

pub fn resolve_path_from_cwd(value: &str, cwd: &Path) -> String {
    if value.trim().is_empty() {
        return value.to_string();
    }
    let path = PathBuf::from(value);
    let absolute = if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    };
    absolute
        .canonicalize()
        .unwrap_or(absolute)
        .to_string_lossy()
        .into_owned()
}

pub fn resolve_optional_path_from_cwd(value: Option<&str>, cwd: &Path) -> Option<String> {
    value.map(|value| resolve_path_from_cwd(value, cwd))
}

pub fn resolve_paths_from_cwd(values: &[String], cwd: &Path) -> Vec<String> {
    values
        .iter()
        .map(|value| resolve_path_from_cwd(value, cwd))
        .collect()
}

fn insert_payload_debug(packet: &mut Value, debug: Value) {
    let Some(payload) = packet.get_mut("payload") else {
        return;
    };
    let Value::Object(map) = payload else {
        return;
    };
    map.insert("debug".to_string(), debug);
}

fn refresh_packet_budget(packet: &mut Value) {
    for _ in 0..5 {
        let payload_bytes = packet
            .get("payload")
            .and_then(|payload| serde_json::to_vec(payload).ok())
            .map(|bytes| bytes.len())
            .unwrap_or(0);
        let packet_bytes = serde_json::to_vec(&*packet)
            .map(|bytes| bytes.len())
            .unwrap_or(0);
        let payload_tokens = suite_packet_core::estimate_tokens_from_bytes(payload_bytes);
        let packet_tokens = suite_packet_core::estimate_tokens_from_bytes(packet_bytes);

        let Some(budget_cost) = packet.get_mut("budget_cost").and_then(Value::as_object_mut) else {
            return;
        };

        let current_packet_bytes = budget_cost
            .get("est_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let current_packet_tokens = budget_cost
            .get("est_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let current_payload_bytes = budget_cost
            .get("payload_est_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let current_payload_tokens = budget_cost
            .get("payload_est_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);

        budget_cost.insert("est_bytes".to_string(), Value::from(packet_bytes as u64));
        budget_cost.insert("est_tokens".to_string(), Value::from(packet_tokens));
        budget_cost.insert(
            "payload_est_bytes".to_string(),
            Value::from(payload_bytes as u64),
        );
        budget_cost.insert(
            "payload_est_tokens".to_string(),
            Value::from(payload_tokens),
        );

        if current_packet_bytes == packet_bytes
            && current_packet_tokens == packet_tokens
            && current_payload_bytes == payload_bytes
            && current_payload_tokens == payload_tokens
        {
            break;
        }
    }
}

fn extract_cache_hit(debug: &Value) -> Option<bool> {
    match debug {
        Value::Object(map) => {
            if let Some(cache) = map.get("cache") {
                if let Some(hit) = find_cache_hit(cache) {
                    return Some(hit);
                }
            }
            for value in map.values() {
                if let Some(hit) = extract_cache_hit(value) {
                    return Some(hit);
                }
            }
            None
        }
        Value::Array(values) => values.iter().find_map(extract_cache_hit),
        _ => None,
    }
}

fn find_cache_hit(value: &Value) -> Option<bool> {
    match value {
        Value::Object(map) => {
            if let Some(hit) = map.get("hit").and_then(Value::as_bool) {
                return Some(hit);
            }
            map.values().find_map(find_cache_hit)
        }
        Value::Array(values) => values.iter().find_map(find_cache_hit),
        _ => None,
    }
}

fn compact_packet_payload(packet_type: &str, packet: &mut Value) {
    let mut stats = CompactStats::default();
    {
        let Some(payload) = packet.get_mut("payload") else {
            return;
        };
        match packet_type {
            suite_packet_core::PACKET_TYPE_CONTEXT_ASSEMBLE => {
                compact_context_assemble_payload(payload, &mut stats)
            }
            suite_packet_core::PACKET_TYPE_DIFF_ANALYZE => {
                compact_diff_payload(payload, &mut stats)
            }
            suite_packet_core::PACKET_TYPE_TEST_IMPACT => {
                compact_test_impact_payload(payload, &mut stats)
            }
            suite_packet_core::PACKET_TYPE_STACK_SLICE => {
                compact_stack_slice_payload(payload, &mut stats)
            }
            suite_packet_core::PACKET_TYPE_BUILD_REDUCE => {
                compact_build_reduce_payload(payload, &mut stats)
            }
            suite_packet_core::PACKET_TYPE_MAP_REPO => {
                compact_map_repo_payload(payload, &mut stats)
            }
            suite_packet_core::PACKET_TYPE_PROXY_RUN => compact_proxy_payload(payload, &mut stats),
            suite_packet_core::PACKET_TYPE_GUARD_CHECK => {
                compact_guard_check_payload(payload, &mut stats)
            }
            suite_packet_core::PACKET_TYPE_COVER_CHECK => {
                compact_cover_check_payload(payload, &mut stats)
            }
            _ => compact_value(payload, 32, &mut stats),
        }
    }
    compact_packet_envelope(packet_type, packet, &mut stats);
    if let Some(Value::Object(map)) = packet.get_mut("payload") {
        if stats.truncated {
            map.insert("truncated".to_string(), Value::Bool(true));
            map.insert(
                "returned_count".to_string(),
                Value::from(stats.returned_count as u64),
            );
            map.insert(
                "total_count".to_string(),
                Value::from(stats.total_count as u64),
            );
        }
    }
}

fn compact_packet_envelope(packet_type: &str, packet: &mut Value, stats: &mut CompactStats) {
    let Some(map) = packet.as_object_mut() else {
        return;
    };

    match packet_type {
        suite_packet_core::PACKET_TYPE_CONTEXT_ASSEMBLE => {
            if let Some(Value::Array(files)) = map.get_mut("files") {
                cap_array(files, 4, stats);
            }
            if let Some(Value::Array(symbols)) = map.get_mut("symbols") {
                cap_array(symbols, 6, stats);
                for symbol in symbols {
                    let Some(symbol_map) = symbol.as_object_mut() else {
                        continue;
                    };
                    truncate_string_field(symbol_map, "name", 96, stats);
                    truncate_string_field(symbol_map, "file", 96, stats);
                }
            }
        }
        suite_packet_core::PACKET_TYPE_STACK_SLICE
        | suite_packet_core::PACKET_TYPE_BUILD_REDUCE
        | suite_packet_core::PACKET_TYPE_MAP_REPO
        | suite_packet_core::PACKET_TYPE_TEST_IMPACT
        | suite_packet_core::PACKET_TYPE_DIFF_ANALYZE => {
            if let Some(Value::Array(files)) = map.get_mut("files") {
                let limit = match packet_type {
                    suite_packet_core::PACKET_TYPE_MAP_REPO => 3,
                    suite_packet_core::PACKET_TYPE_STACK_SLICE
                    | suite_packet_core::PACKET_TYPE_BUILD_REDUCE => 2,
                    _ => 8,
                };
                cap_array(files, limit, stats);
            }
            if let Some(Value::Array(symbols)) = map.get_mut("symbols") {
                let limit = match packet_type {
                    suite_packet_core::PACKET_TYPE_MAP_REPO => 4,
                    suite_packet_core::PACKET_TYPE_STACK_SLICE
                    | suite_packet_core::PACKET_TYPE_BUILD_REDUCE => 2,
                    _ => 8,
                };
                cap_array(symbols, limit, stats);
            }
        }
        _ => {}
    }
}

fn compact_context_assemble_payload(payload: &mut Value, stats: &mut CompactStats) {
    let Some(map) = payload.as_object_mut() else {
        compact_value(payload, 12, stats);
        return;
    };

    remove_field(map, "debug", stats);
    remove_field(map, "text_blobs", stats);
    remove_field(map, "tool_invocations", stats);
    remove_field(map, "reducer_invocations", stats);

    if let Some(Value::Array(sections)) = map.get_mut("sections") {
        cap_array(sections, 4, stats);
        for section in sections {
            let Some(section_map) = section.as_object_mut() else {
                continue;
            };
            remove_field(section_map, "id", stats);
            remove_field(section_map, "refs", stats);
            remove_field(section_map, "relevance", stats);
            truncate_string_field(section_map, "title", 80, stats);
            truncate_string_field(section_map, "source_packet", 64, stats);
            if let Some(Value::String(body)) = section_map.get_mut("body") {
                *body = summarize_text_body(body, 144, stats);
            }
        }
    }

    if let Some(Value::Array(refs)) = map.get_mut("refs") {
        cap_array(refs, 8, stats);
        for reference in refs {
            let Some(reference_map) = reference.as_object_mut() else {
                continue;
            };
            remove_field(reference_map, "source", stats);
            remove_field(reference_map, "relevance", stats);
            truncate_string_field(reference_map, "kind", 24, stats);
            truncate_string_field(reference_map, "value", 96, stats);
        }
    }

    remove_field(map, "sources", stats);

    if let Some(Value::Object(assembly)) = map.get_mut("assembly") {
        let keep = [
            "estimated_tokens",
            "input_packets",
            "refs_kept",
            "sections_kept",
            "truncated",
        ];
        let remove = assembly
            .keys()
            .filter(|key| !keep.contains(&key.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        for key in remove {
            assembly.remove(&key);
            stats.truncated = true;
            stats.total_count += 1;
        }
    }
}

fn compact_diff_payload(payload: &mut Value, stats: &mut CompactStats) {
    let Some(map) = payload.as_object_mut() else {
        compact_value(payload, 16, stats);
        return;
    };
    remove_field(map, "debug", stats);
    if let Some(Value::Array(diffs)) = map.get_mut("diffs") {
        cap_array(diffs, 6, stats);
        for diff in diffs {
            let Some(diff_map) = diff.as_object_mut() else {
                continue;
            };
            truncate_string_field(diff_map, "path", 96, stats);
            if let Some(Value::Array(lines)) = diff_map.get_mut("changed_lines") {
                cap_array(lines, 8, stats);
            }
        }
    }
    if let Some(diag) = map.get_mut("diagnostics") {
        compact_value(diag, 6, stats);
    }
}

fn compact_test_impact_payload(payload: &mut Value, stats: &mut CompactStats) {
    let Some(map) = payload.as_object_mut() else {
        compact_value(payload, 16, stats);
        return;
    };
    remove_field(map, "debug", stats);
    truncate_string_field(map, "print_command", 120, stats);
    if let Some(Value::Object(result)) = map.get_mut("result") {
        if let Some(Value::Array(items)) = result.get_mut("selected_tests") {
            cap_array(items, 8, stats);
        }
        if let Some(Value::Array(items)) = result.get_mut("missing_mappings") {
            cap_array(items, 8, stats);
        }
        if let Some(Value::Array(items)) = result.get_mut("smoke_tests") {
            cap_array(items, 8, stats);
        }
    }
}

fn compact_stack_slice_payload(payload: &mut Value, stats: &mut CompactStats) {
    let Some(map) = payload.as_object_mut() else {
        compact_value(payload, 12, stats);
        return;
    };
    remove_field(map, "debug", stats);
    remove_field(map, "schema_version", stats);
    remove_field(map, "source", stats);
    remove_field(map, "duplicates_removed", stats);
    if let Some(Value::Array(failures)) = map.get_mut("failures") {
        cap_array(failures, 4, stats);
        for failure in failures {
            if let Some(failure_map) = failure.as_object_mut() {
                remove_field(failure_map, "fingerprint", stats);
                truncate_string_field(failure_map, "title", 96, stats);
                remove_field(failure_map, "message", stats);
                remove_field(failure_map, "frames", stats);
                if let Some(Value::Object(frame_map)) =
                    failure_map.get_mut("first_actionable_frame")
                {
                    remove_field(frame_map, "actionable", stats);
                    remove_field(frame_map, "raw", stats);
                    remove_field(frame_map, "normalized", stats);
                    truncate_string_field(frame_map, "function", 72, stats);
                    truncate_string_field(frame_map, "file", 96, stats);
                }
            }
        }
    }
    remove_field(map, "unique_failures", stats);
}

fn compact_build_reduce_payload(payload: &mut Value, stats: &mut CompactStats) {
    let Some(map) = payload.as_object_mut() else {
        compact_value(payload, 12, stats);
        return;
    };
    remove_field(map, "debug", stats);
    remove_field(map, "schema_version", stats);
    remove_field(map, "source", stats);
    remove_field(map, "duplicates_removed", stats);
    if let Some(Value::Array(groups)) = map.get_mut("groups") {
        cap_array(groups, 2, stats);
        for group in groups {
            let Some(group_map) = group.as_object_mut() else {
                continue;
            };
            truncate_string_field(group_map, "root_cause", 96, stats);
            remove_field(group_map, "diagnostics", stats);
        }
    }
    if let Some(Value::Array(fixes)) = map.get_mut("ordered_fixes") {
        cap_array(fixes, 1, stats);
        for fix in fixes {
            if let Some(text) = fix.as_str() {
                *fix = Value::String(summarize_text_body(text, 96, stats));
            }
        }
    }
}

fn compact_map_repo_payload(payload: &mut Value, stats: &mut CompactStats) {
    let Some(map) = payload.as_object_mut() else {
        compact_value(payload, 12, stats);
        return;
    };
    remove_field(map, "debug", stats);
    if let Some(Value::Array(files)) = map.get_mut("files_ranked") {
        cap_array(files, 2, stats);
        for file in files {
            let Some(file_map) = file.as_object_mut() else {
                continue;
            };
            remove_field(file_map, "import_count", stats);
            remove_field(file_map, "symbol_count", stats);
            remove_field(file_map, "score", stats);
        }
    }
    if let Some(Value::Array(symbols)) = map.get_mut("symbols_ranked") {
        cap_array(symbols, 4, stats);
        for symbol in symbols {
            let Some(symbol_map) = symbol.as_object_mut() else {
                continue;
            };
            remove_field(symbol_map, "file_idx", stats);
            remove_field(symbol_map, "score", stats);
        }
    }
    if let Some(Value::Array(edges)) = map.get_mut("edges") {
        if edges.is_empty() {
            map.remove("edges");
            stats.truncated = true;
            stats.total_count += 1;
        } else {
            cap_array(edges, 4, stats);
        }
    }
    if let Some(Value::Array(hits)) = map.get_mut("focus_hits") {
        cap_array(hits, 2, stats);
        for hit in hits {
            let Some(hit_map) = hit.as_object_mut() else {
                continue;
            };
            remove_field(hit_map, "kind", stats);
        }
    }
    if let Some(Value::Object(truncation)) = map.get_mut("truncation") {
        let all_zero = truncation
            .values()
            .all(|value| value.as_u64().unwrap_or_default() == 0);
        if all_zero {
            map.remove("truncation");
            stats.truncated = true;
            stats.total_count += 1;
        }
    }
}

fn compact_proxy_payload(payload: &mut Value, stats: &mut CompactStats) {
    let Some(map) = payload.as_object_mut() else {
        compact_value(payload, 12, stats);
        return;
    };
    remove_field(map, "debug", stats);
    if let Some(Value::Array(groups)) = map.get_mut("groups") {
        cap_array(groups, 4, stats);
    }
    if let Some(Value::Array(highlights)) = map.get_mut("highlights") {
        cap_array(highlights, 4, stats);
        for highlight in highlights {
            if let Some(text) = highlight.as_str() {
                *highlight = Value::String(summarize_text_body(text, 160, stats));
            }
        }
    }
    remove_field(map, "output_lines", stats);
}

fn compact_guard_check_payload(payload: &mut Value, stats: &mut CompactStats) {
    let Some(map) = payload.as_object_mut() else {
        compact_value(payload, 12, stats);
        return;
    };
    remove_field(map, "debug", stats);
    if let Some(Value::Array(findings)) = map.get_mut("findings") {
        cap_array(findings, 6, stats);
        for finding in findings {
            if let Some(finding_map) = finding.as_object_mut() {
                truncate_string_field(finding_map, "rule", 72, stats);
                truncate_string_field(finding_map, "message", 120, stats);
                truncate_string_field(finding_map, "path", 96, stats);
            }
        }
    }
}

fn compact_cover_check_payload(payload: &mut Value, stats: &mut CompactStats) {
    let Some(map) = payload.as_object_mut() else {
        compact_value(payload, 12, stats);
        return;
    };
    if let Some(Value::Array(violations)) = map.get_mut("violations") {
        cap_array(violations, 6, stats);
        for violation in violations {
            if let Some(text) = violation.as_str() {
                *violation = Value::String(summarize_text_body(text, 120, stats));
            }
        }
    }
}

fn remove_field(map: &mut serde_json::Map<String, Value>, key: &str, stats: &mut CompactStats) {
    if map.remove(key).is_some() {
        stats.truncated = true;
    }
}

fn truncate_string_field(
    map: &mut serde_json::Map<String, Value>,
    key: &str,
    cap: usize,
    stats: &mut CompactStats,
) {
    let Some(Value::String(text)) = map.get_mut(key) else {
        return;
    };
    *text = summarize_text_body(text, cap, stats);
}

fn cap_array(items: &mut Vec<Value>, cap: usize, stats: &mut CompactStats) {
    let total = items.len();
    if total > cap {
        items.truncate(cap);
        stats.truncated = true;
        stats.total_count = stats.total_count.saturating_add(total);
        stats.returned_count = stats.returned_count.saturating_add(cap);
    } else {
        stats.total_count = stats.total_count.saturating_add(total);
        stats.returned_count = stats.returned_count.saturating_add(total);
    }
}

fn summarize_text_body(text: &str, cap: usize, stats: &mut CompactStats) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= cap {
        return normalized;
    }

    stats.truncated = true;
    let mut shortened = normalized.chars().take(cap).collect::<String>();
    shortened = shortened.trim_end().to_string();
    shortened.push_str(" ...");
    shortened
}

fn attach_artifact_handle(packet: &mut Value, handle: Value) {
    let Some(payload) = packet.get_mut("payload") else {
        return;
    };
    if let Value::Object(map) = payload {
        map.insert("artifact_handle".to_string(), handle);
    }
}

#[derive(Default)]
struct CompactStats {
    truncated: bool,
    returned_count: usize,
    total_count: usize,
}

fn compact_value(value: &mut Value, cap: usize, stats: &mut CompactStats) {
    match value {
        Value::Array(items) => {
            let total = items.len();
            if total > cap {
                items.truncate(cap);
                stats.truncated = true;
                stats.total_count = stats.total_count.saturating_add(total);
                stats.returned_count = stats.returned_count.saturating_add(cap);
            } else {
                stats.total_count = stats.total_count.saturating_add(total);
                stats.returned_count = stats.returned_count.saturating_add(total);
            }
            for item in items {
                compact_value(item, cap, stats);
            }
        }
        Value::Object(map) => {
            for value in map.values_mut() {
                compact_value(value, cap, stats);
            }
        }
        _ => {}
    }
}

pub fn default_pipeline_ingest_adapters() -> diffy_core::pipeline::PipelineIngestAdapters {
    diffy_core::pipeline::PipelineIngestAdapters {
        ingest_coverage_auto,
        ingest_coverage_with_format,
        ingest_coverage_stdin,
        ingest_diagnostics,
    }
}

pub fn repo_cache_fingerprint(repo_root: &Path, relevant_paths: &[PathBuf]) -> String {
    suite_foundation_core::repo_fingerprint::cache_fingerprint(repo_root, relevant_paths)
}

fn ingest_coverage_auto(path: &Path) -> Result<CoverageData> {
    suite_ingest::ingest_coverage_path(path, None).map_err(Into::into)
}

fn ingest_coverage_with_format(path: &Path, format: CoverageFormat) -> Result<CoverageData> {
    suite_ingest::ingest_coverage_path(path, Some(format)).map_err(Into::into)
}

fn ingest_coverage_stdin(format: CoverageFormat) -> Result<CoverageData> {
    suite_ingest::ingest_coverage_stdin(format).map_err(Into::into)
}

fn ingest_diagnostics(path: &Path) -> Result<suite_packet_core::diagnostics::DiagnosticsData> {
    suite_ingest::ingest_diagnostics_path(path).map_err(Into::into)
}

pub fn cache_summary_line(metadata: &Value) -> Option<String> {
    let cache = metadata.get("cache")?;
    let hit = cache.get("hit").and_then(Value::as_bool).unwrap_or(false);
    let key = cache
        .get("key")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let age = cache.get("entry_age_secs").and_then(Value::as_u64);
    let miss_reason = cache.get("miss_reason").and_then(Value::as_str);

    if hit {
        Some(format!(
            "cache: hit key={} age={}s",
            key,
            age.unwrap_or_default()
        ))
    } else if let Some(reason) = miss_reason {
        Some(format!("cache: miss key={} reason={}", key, reason))
    } else {
        Some(format!("cache: miss key={}", key))
    }
}

pub fn budget_retry_hint(
    governed_metadata: &Value,
    current_tokens: u64,
    current_bytes: usize,
    retry_command: &str,
) -> Option<Value> {
    let trim = governed_metadata.get("budget_trim")?;
    let truncated = trim
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !truncated {
        return None;
    }

    let sections_dropped = trim
        .get("sections_dropped")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let refs_dropped = trim
        .get("refs_dropped")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let dropped_total = sections_dropped.saturating_add(refs_dropped);

    let sections_input = trim
        .get("sections_input")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let refs_input = trim.get("refs_input").and_then(Value::as_u64).unwrap_or(0);
    let inputs_total = sections_input.saturating_add(refs_input);
    let dropped_ratio = if inputs_total == 0 {
        1.0
    } else {
        dropped_total as f64 / inputs_total as f64
    };

    if dropped_total < 3 && dropped_ratio < 0.30 {
        return None;
    }

    let est_tokens = trim
        .get("estimated_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(current_tokens);
    let est_bytes = trim
        .get("estimated_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(current_bytes as u64);

    let suggested_tokens = round_up_u64(
        ((current_tokens as f64 * 1.5).max(est_tokens as f64 * 1.2)).ceil() as u64,
        250,
    );
    let suggested_bytes = round_up_usize(
        ((current_bytes as f64 * 1.5).max(est_bytes as f64 * 1.2)).ceil() as usize,
        1024,
    );

    Some(json!({
        "reason": "high_truncation",
        "dropped_total": dropped_total,
        "dropped_ratio": dropped_ratio,
        "suggested_context_budget_tokens": suggested_tokens,
        "suggested_context_budget_bytes": suggested_bytes,
        "retry_command": format!(
            "{} --context-budget-tokens {} --context-budget-bytes {}",
            retry_command, suggested_tokens, suggested_bytes
        ),
    }))
}

fn round_up_u64(value: u64, step: u64) -> u64 {
    if step == 0 {
        return value;
    }
    value.div_ceil(step) * step
}

fn round_up_usize(value: usize, step: usize) -> usize {
    if step == 0 {
        return value;
    }
    value.div_ceil(step) * step
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_context_assemble_payload_drops_duplicate_bulk_fields() {
        let mut packet = json!({
            "payload": {
                "sections": [
                    {
                        "title": "Map",
                        "body": "x".repeat(400),
                        "refs": [{"kind": "file", "value": "src/lib.rs"}],
                        "relevance": 0.9,
                        "id": "sec-1"
                    }
                ],
                "refs": [
                    {"kind": "file", "value": "src/lib.rs"},
                    {"kind": "file", "value": "src/main.rs"}
                ],
                "sources": ["a", "b"],
                "tool_invocations": [{"name": "mapy"}],
                "reducer_invocations": [{"name": "contextq.assemble"}],
                "text_blobs": ["x".repeat(400)],
                "debug": {"cache": {"hit": false}}
            }
        });

        compact_packet_payload(suite_packet_core::PACKET_TYPE_CONTEXT_ASSEMBLE, &mut packet);
        let payload = packet.get("payload").and_then(Value::as_object).unwrap();

        assert!(payload.get("text_blobs").is_none());
        assert!(payload.get("tool_invocations").is_none());
        assert!(payload.get("reducer_invocations").is_none());
        assert!(payload.get("debug").is_none());
        assert!(payload
            .get("truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false));

        let section = payload
            .get("sections")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(Value::as_object)
            .unwrap();
        assert!(section.get("refs").is_none());
        assert!(section.get("body").and_then(Value::as_str).unwrap().len() < 260);
    }

    #[test]
    fn compact_handle_path_does_not_keep_debug_payload() {
        let envelope = EnvelopeV1 {
            version: "1".to_string(),
            tool: "suite".to_string(),
            kind: "demo".to_string(),
            hash: String::new(),
            summary: "demo".to_string(),
            files: Vec::new(),
            symbols: Vec::new(),
            risk: None,
            confidence: Some(1.0),
            budget_cost: suite_packet_core::BudgetCost {
                est_tokens: 0,
                est_bytes: 0,
                runtime_ms: 0,
                tool_calls: 1,
                payload_est_tokens: Some(10),
                payload_est_bytes: Some(40),
            },
            provenance: suite_packet_core::Provenance {
                inputs: vec!["input".to_string()],
                git_base: None,
                git_head: None,
                generated_at_unix: 1,
            },
            payload: json!({
                "command": "echo ok",
                "highlights": ["ok"],
                "output_lines": ["ok"]
            }),
        }
        .with_canonical_hash_and_real_budget();

        let mut packet = serde_json::to_value(&envelope).unwrap();
        compact_packet_payload(suite_packet_core::PACKET_TYPE_PROXY_RUN, &mut packet);
        assert!(packet
            .get("payload")
            .and_then(Value::as_object)
            .unwrap()
            .get("debug")
            .is_none());
    }
}
