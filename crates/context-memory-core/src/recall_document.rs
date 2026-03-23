use super::*;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub(crate) struct RecallDocument {
    pub(crate) cache_key: String,
    pub(crate) target: String,
    pub(crate) created_at_unix: u64,
    pub(crate) summary: Option<String>,
    pub(crate) snippet: String,
    pub(crate) task_ids: Vec<String>,
    pub(crate) packet_types: Vec<String>,
    pub(crate) paths: Vec<String>,
    pub(crate) path_basenames: Vec<String>,
    pub(crate) symbols: Vec<String>,
    pub(crate) tests: Vec<String>,
    pub(crate) terms: HashMap<String, usize>,
    pub(crate) doc_length: usize,
    pub(crate) budget_estimate: RecallBudgetEstimate,
}

pub(crate) fn build_recall_document(
    entry: &PacketCacheEntry,
    workspace_root: Option<&Path>,
) -> RecallDocument {
    let mut corpus = Vec::new();
    let mut summaries = Vec::new();
    let mut path_terms = Vec::new();
    let mut path_basenames = Vec::new();
    let mut symbol_terms = Vec::new();
    let mut test_terms = Vec::new();
    let mut packet_types = BTreeSet::new();
    let mut task_ids = BTreeSet::new();
    let mut budget_estimate = RecallBudgetEstimate::default();

    for packet in &entry.packets {
        collect_summary_texts(&packet.body, &mut summaries, 24);
        collect_summary_texts(&packet.metadata, &mut summaries, 24);
        if let Some(summary) = extract_packet_summary(&packet.body) {
            push_unique_text(&mut summaries, &summary, 24);
        }
        collect_texts_from_value(&packet.body, &mut corpus, 160);
        collect_texts_from_value(&packet.metadata, &mut corpus, 96);
        collect_ref_terms(
            &packet.body,
            workspace_root,
            &mut path_terms,
            &mut path_basenames,
            &mut symbol_terms,
            &mut test_terms,
        );
        collect_ref_terms(
            &packet.metadata,
            workspace_root,
            &mut path_terms,
            &mut path_basenames,
            &mut symbol_terms,
            &mut test_terms,
        );
        collect_task_ids(&packet.body, &mut task_ids);
        collect_task_ids(&packet.metadata, &mut task_ids);
        if let Some(packet_type) = packet
            .body
            .get("packet_type")
            .and_then(Value::as_str)
            .or_else(|| packet.body.get("kind").and_then(Value::as_str))
        {
            packet_types.insert(packet_type.to_ascii_lowercase());
        }
        let packet_budget =
            extract_budget_estimate(&packet.body, packet.token_usage, packet.runtime_ms);
        budget_estimate.est_tokens = budget_estimate
            .est_tokens
            .saturating_add(packet_budget.est_tokens);
        budget_estimate.est_bytes = budget_estimate
            .est_bytes
            .saturating_add(packet_budget.est_bytes);
        budget_estimate.runtime_ms = budget_estimate
            .runtime_ms
            .saturating_add(packet_budget.runtime_ms);
    }

    collect_summary_texts(&entry.metadata, &mut summaries, 24);
    collect_texts_from_value(&entry.metadata, &mut corpus, 96);
    collect_task_ids(&entry.metadata, &mut task_ids);
    corpus.extend(summaries.iter().cloned());
    corpus.push(entry.target.clone());
    corpus.push(entry.cache_key.clone());
    corpus.push(entry.input_hash.clone());
    corpus.extend(path_terms.iter().cloned());
    corpus.extend(path_basenames.iter().cloned());
    corpus.extend(symbol_terms.iter().cloned());
    corpus.extend(test_terms.iter().cloned());

    let summary = select_recall_summary(&summaries)
        .or_else(|| {
            corpus
                .iter()
                .filter(|item| !item.trim().is_empty())
                .max_by_key(|item| recall_summary_priority(item))
                .cloned()
        })
        .map(|item| truncate_recall_text(item, 200));
    let snippet = summary
        .clone()
        .or_else(|| corpus.iter().find(|item| !item.trim().is_empty()).cloned())
        .map(|item| truncate_recall_text(item, 200))
        .unwrap_or_else(|| "{}".to_string());

    let mut terms = HashMap::<String, usize>::new();
    let mut doc_length = 0_usize;
    for item in &corpus {
        for term in tokenize(item) {
            *terms.entry(term).or_insert(0) += 1;
            doc_length = doc_length.saturating_add(1);
        }
    }

    RecallDocument {
        cache_key: entry.cache_key.clone(),
        target: entry.target.clone(),
        created_at_unix: entry.created_at_unix,
        summary,
        snippet,
        task_ids: task_ids
            .into_iter()
            .map(|item| item.to_ascii_lowercase())
            .collect(),
        packet_types: packet_types.into_iter().collect(),
        paths: path_terms,
        path_basenames,
        symbols: symbol_terms,
        tests: test_terms,
        terms,
        doc_length,
        budget_estimate,
    }
}

pub(crate) fn extract_budget_estimate(
    body: &Value,
    token_usage: Option<u64>,
    runtime_ms: Option<u64>,
) -> RecallBudgetEstimate {
    let budget = body.get("budget_cost").and_then(Value::as_object);
    RecallBudgetEstimate {
        est_tokens: budget
            .and_then(|value| value.get("est_tokens"))
            .and_then(Value::as_u64)
            .or(token_usage)
            .unwrap_or_default(),
        est_bytes: budget
            .and_then(|value| value.get("est_bytes"))
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        runtime_ms: budget
            .and_then(|value| value.get("runtime_ms"))
            .and_then(Value::as_u64)
            .or(runtime_ms)
            .unwrap_or_default(),
    }
}

pub(crate) fn select_recall_summary(candidates: &[String]) -> Option<String> {
    candidates
        .iter()
        .filter(|item| !item.trim().is_empty())
        .max_by_key(|item| recall_summary_priority(item))
        .cloned()
}

pub(crate) fn recall_summary_priority(text: &str) -> i32 {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return i32::MIN;
    }

    let mut score = 0_i32;
    if !looks_like_path(trimmed) {
        score += 40;
    } else {
        score -= 40;
    }

    let word_count = trimmed.split_whitespace().count() as i32;
    score += word_count.min(12) * 2;
    score += (trimmed.len().min(180) as i32) / 12;

    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("context assemble ") || lower.starts_with("assembled context ") {
        score -= 60;
    }
    if lower.starts_with("repo map files=")
        || lower.starts_with("diff touched files=")
        || lower.starts_with("test impact selected_tests=")
        || lower.starts_with("build diagnostics unique=")
        || lower.starts_with("correlation findings=")
        || lower.starts_with("agent snapshot events=")
    {
        score -= 24;
    }
    if lower.contains("truncated=") && lower.contains("sections=") && lower.contains("refs=") {
        score -= 24;
    }

    score
}

pub(crate) fn extract_packet_summary(body: &Value) -> Option<String> {
    let tool = body.get("tool").and_then(Value::as_str).unwrap_or_default();
    let kind = body.get("kind").and_then(Value::as_str).unwrap_or_default();
    let payload = body.get("payload").unwrap_or(body);
    match (tool, kind) {
        ("stacky", "stack_slice") => Some(format!(
            "stack failures={} unique={}",
            payload
                .get("total_failures")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            payload
                .get("unique_failures")
                .and_then(Value::as_u64)
                .unwrap_or_default()
        )),
        ("mapy", "repo_map") => Some(format!(
            "repo map files={} symbols={}",
            payload
                .get("files_ranked")
                .and_then(Value::as_array)
                .map(|items| items.len())
                .unwrap_or_default(),
            payload
                .get("symbols_ranked")
                .and_then(Value::as_array)
                .map(|items| items.len())
                .unwrap_or_default()
        )),
        ("mapy", "repo_query") => Some(format!(
            "repo query matches={} query={}",
            payload
                .get("matches")
                .and_then(Value::as_array)
                .map(|items| items.len())
                .unwrap_or_default(),
            payload
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or_default()
        )),
        ("diffy", "diff_analyze") => Some(format!(
            "diff touched files={}",
            payload
                .get("diffs")
                .and_then(Value::as_array)
                .map(|items| items.len())
                .unwrap_or_default()
        )),
        ("testy", "test_impact") => Some(format!(
            "test impact selected_tests={}",
            payload
                .get("result")
                .and_then(|value| value.get("selected_tests"))
                .and_then(Value::as_array)
                .map(|items| items.len())
                .unwrap_or_default()
        )),
        ("buildy", "build_reduce") => Some(format!(
            "build diagnostics unique={}",
            payload
                .get("unique_diagnostics")
                .and_then(Value::as_u64)
                .unwrap_or_default()
        )),
        ("contextq", "context_correlate") => Some(format!(
            "correlation findings={}",
            payload
                .get("finding_count")
                .and_then(Value::as_u64)
                .unwrap_or_default()
        )),
        ("agenty", "agent_snapshot") => Some(format!(
            "agent snapshot events={} questions={}",
            payload
                .get("event_count")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            payload
                .get("open_questions")
                .and_then(Value::as_array)
                .map(|items| items.len())
                .unwrap_or_default()
        )),
        ("contextq", "context_assemble") => Some(format!(
            "assembled context sections={} refs={} truncated={}",
            payload
                .get("sections")
                .and_then(Value::as_array)
                .map(|items| items.len())
                .unwrap_or_default(),
            payload
                .get("refs")
                .and_then(Value::as_array)
                .map(|items| items.len())
                .unwrap_or_default(),
            payload
                .get("truncated")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        )),
        _ => None,
    }
}

pub(crate) fn collect_task_ids(value: &Value, out: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            if let Some(task_id) = map.get("task_id").and_then(Value::as_str) {
                let task_id = task_id.trim();
                if !task_id.is_empty() {
                    out.insert(task_id.to_string());
                }
            }
            for child in map.values() {
                collect_task_ids(child, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_task_ids(item, out);
            }
        }
        _ => {}
    }
}

pub(crate) fn looks_like_path(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.contains('/') || trimmed.contains('\\') {
        return true;
    }
    let Some((stem, ext)) = trimmed.rsplit_once('.') else {
        return false;
    };
    !stem.is_empty()
        && !ext.is_empty()
        && ext.len() <= 4
        && ext.chars().all(|ch| ch.is_ascii_alphanumeric())
        && ext.chars().any(|ch| ch.is_ascii_alphabetic())
}

pub(crate) fn collect_summary_texts(value: &Value, out: &mut Vec<String>, max_items: usize) {
    if out.len() >= max_items {
        return;
    }

    match value {
        Value::Object(map) => {
            if let Some(summary) = map.get("summary").and_then(Value::as_str) {
                push_unique_text(out, summary, max_items);
            }
            if (map.contains_key("title") || map.contains_key("source_packet"))
                && map
                    .get("body")
                    .and_then(Value::as_str)
                    .is_some_and(|body| !body.trim().is_empty())
            {
                push_unique_text(
                    out,
                    map.get("body").and_then(Value::as_str).unwrap_or_default(),
                    max_items,
                );
            }
            for child in map.values() {
                collect_summary_texts(child, out, max_items);
                if out.len() >= max_items {
                    return;
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_summary_texts(item, out, max_items);
                if out.len() >= max_items {
                    return;
                }
            }
        }
        _ => {}
    }
}

pub(crate) fn push_unique_text(out: &mut Vec<String>, text: &str, max_items: usize) {
    if out.len() >= max_items {
        return;
    }

    let trimmed = text.trim();
    if trimmed.is_empty() || out.iter().any(|existing| existing == trimmed) {
        return;
    }
    out.push(trimmed.to_string());
}

pub(crate) fn truncate_recall_text(mut text: String, max_len: usize) -> String {
    if text.chars().count() > max_len {
        text = text.chars().take(max_len).collect();
    }
    text
}

pub(crate) fn collect_texts_from_value(value: &Value, out: &mut Vec<String>, max_items: usize) {
    if out.len() >= max_items {
        return;
    }
    match value {
        Value::String(text) => {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                out.push(trimmed.to_string());
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_texts_from_value(item, out, max_items);
                if out.len() >= max_items {
                    return;
                }
            }
        }
        Value::Object(map) => {
            for value in map.values() {
                collect_texts_from_value(value, out, max_items);
                if out.len() >= max_items {
                    return;
                }
            }
        }
        _ => {}
    }
}

pub(crate) fn collect_ref_terms(
    value: &Value,
    workspace_root: Option<&Path>,
    paths: &mut Vec<String>,
    path_basenames: &mut Vec<String>,
    symbols: &mut Vec<String>,
    tests: &mut Vec<String>,
) {
    match value {
        Value::Object(map) => {
            if let Some(path) = map.get("path").and_then(Value::as_str) {
                push_normalized_path(paths, path_basenames, path, workspace_root);
            }
            if let Some(file) = map.get("file").and_then(Value::as_str) {
                push_normalized_path(paths, path_basenames, file, workspace_root);
            }
            if let Some(name) = map.get("name").and_then(Value::as_str) {
                push_unique_text(symbols, &name.to_ascii_lowercase(), usize::MAX);
            }
            if let Some(test_id) = map.get("test_id").and_then(Value::as_str) {
                push_unique_text(tests, &test_id.to_ascii_lowercase(), usize::MAX);
            }
            if map
                .get("kind")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind.eq_ignore_ascii_case("file"))
            {
                if let Some(value) = map.get("value").and_then(Value::as_str) {
                    push_normalized_path(paths, path_basenames, value, workspace_root);
                }
            }
            if map
                .get("kind")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind.eq_ignore_ascii_case("symbol"))
            {
                if let Some(value) = map.get("value").and_then(Value::as_str) {
                    push_unique_text(symbols, &value.to_ascii_lowercase(), usize::MAX);
                }
            }
            if let Some(selected_tests) = map.get("selected_tests").and_then(Value::as_array) {
                for test in selected_tests {
                    if let Some(test) = test.as_str() {
                        push_unique_text(tests, &test.to_ascii_lowercase(), usize::MAX);
                    }
                }
            }
            for child in map.values() {
                collect_ref_terms(child, workspace_root, paths, path_basenames, symbols, tests);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_ref_terms(item, workspace_root, paths, path_basenames, symbols, tests);
            }
        }
        Value::String(text) => {
            if looks_like_path(text) {
                push_normalized_path(paths, path_basenames, text, workspace_root);
            } else if text.contains("::") {
                push_unique_text(symbols, &text.to_ascii_lowercase(), usize::MAX);
            }
        }
        _ => {}
    }
}
