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

impl PacketCache {
    pub fn related_entries(
        &self,
        task_id: Option<&str>,
        canonical_paths: &[String],
        symbols: &[String],
        tests: &[String],
    ) -> Vec<RelatedEntryMatch> {
        let task_filter = task_id.map(|value| value.to_ascii_lowercase());
        let task_keys = task_filter
            .as_ref()
            .and_then(|task_id| self.task_index.get(task_id))
            .cloned();
        let mut matches = HashMap::<String, RelatedEntryMatch>::new();

        for path in canonical_paths {
            if let Some(cache_keys) = self.file_ref_index.get(path) {
                for cache_key in cache_keys {
                    if !task_match_allowed(cache_key, task_keys.as_ref()) {
                        continue;
                    }
                    if let Some(entry) = self.entries_by_hash.get(cache_key).cloned() {
                        let item =
                            matches
                                .entry(cache_key.clone())
                                .or_insert_with(|| RelatedEntryMatch {
                                    entry,
                                    canonical_path_matches: Vec::new(),
                                    basename_path_matches: Vec::new(),
                                    symbol_matches: Vec::new(),
                                    test_matches: Vec::new(),
                                });
                        if !item
                            .canonical_path_matches
                            .iter()
                            .any(|existing| existing == path)
                        {
                            item.canonical_path_matches.push(path.clone());
                        }
                    }
                }
            } else if let Some(basename) = basename_alias(path) {
                if let Some(canonicals) = self.basename_alias_index.get(&basename) {
                    if canonicals.len() == 1 {
                        for canonical in canonicals {
                            if let Some(cache_keys) = self.file_ref_index.get(canonical) {
                                for cache_key in cache_keys {
                                    if !task_match_allowed(cache_key, task_keys.as_ref()) {
                                        continue;
                                    }
                                    if let Some(entry) =
                                        self.entries_by_hash.get(cache_key).cloned()
                                    {
                                        let item =
                                            matches.entry(cache_key.clone()).or_insert_with(|| {
                                                RelatedEntryMatch {
                                                    entry,
                                                    canonical_path_matches: Vec::new(),
                                                    basename_path_matches: Vec::new(),
                                                    symbol_matches: Vec::new(),
                                                    test_matches: Vec::new(),
                                                }
                                            });
                                        if !item
                                            .basename_path_matches
                                            .iter()
                                            .any(|existing| existing == canonical)
                                        {
                                            item.basename_path_matches.push(canonical.clone());
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        for symbol in symbols {
            let symbol = symbol.to_ascii_lowercase();
            for (candidate, cache_keys) in &self.symbol_index {
                if candidate == &symbol
                    || candidate.starts_with(&symbol)
                    || candidate.contains(&symbol)
                {
                    for cache_key in cache_keys {
                        if !task_match_allowed(cache_key, task_keys.as_ref()) {
                            continue;
                        }
                        if let Some(entry) = self.entries_by_hash.get(cache_key).cloned() {
                            let item = matches.entry(cache_key.clone()).or_insert_with(|| {
                                RelatedEntryMatch {
                                    entry,
                                    canonical_path_matches: Vec::new(),
                                    basename_path_matches: Vec::new(),
                                    symbol_matches: Vec::new(),
                                    test_matches: Vec::new(),
                                }
                            });
                            if !item
                                .symbol_matches
                                .iter()
                                .any(|existing| existing == candidate)
                            {
                                item.symbol_matches.push(candidate.clone());
                            }
                        }
                    }
                }
            }
        }

        for test in tests {
            let test = test.to_ascii_lowercase();
            for (candidate, cache_keys) in &self.test_index {
                if candidate == &test || candidate.starts_with(&test) || candidate.contains(&test) {
                    for cache_key in cache_keys {
                        if !task_match_allowed(cache_key, task_keys.as_ref()) {
                            continue;
                        }
                        if let Some(entry) = self.entries_by_hash.get(cache_key).cloned() {
                            let item = matches.entry(cache_key.clone()).or_insert_with(|| {
                                RelatedEntryMatch {
                                    entry,
                                    canonical_path_matches: Vec::new(),
                                    basename_path_matches: Vec::new(),
                                    symbol_matches: Vec::new(),
                                    test_matches: Vec::new(),
                                }
                            });
                            if !item
                                .test_matches
                                .iter()
                                .any(|existing| existing == candidate)
                            {
                                item.test_matches.push(candidate.clone());
                            }
                        }
                    }
                }
            }
        }

        let mut values = matches.into_values().collect::<Vec<_>>();
        values.sort_by(|a, b| a.entry.cache_key.cmp(&b.entry.cache_key));
        values
    }

    pub fn recall(&self, query: &str, options: &RecallOptions) -> Vec<RecallHit> {
        let query_tokens = tokenize(query);
        if query_tokens.is_empty()
            && options.packet_types.is_empty()
            && options.path_filters.is_empty()
            && options.symbol_filters.is_empty()
        {
            return Vec::new();
        }

        let now = now_unix();
        let target_filter = options.target.as_ref().map(|v| v.to_ascii_lowercase());
        let packet_type_filters = options
            .packet_types
            .iter()
            .map(|item| item.to_ascii_lowercase())
            .collect::<Vec<_>>();
        let path_filters = options
            .path_filters
            .iter()
            .map(|item| item.to_ascii_lowercase())
            .collect::<Vec<_>>();
        let symbol_filters = options
            .symbol_filters
            .iter()
            .map(|item| item.to_ascii_lowercase())
            .collect::<Vec<_>>();
        let task_filter = options
            .task_id
            .as_ref()
            .map(|task| task.to_ascii_lowercase());
        let query_path_terms = extract_query_path_terms(query);
        let mut candidate_scores = HashMap::<String, f64>::new();
        let mut canonical_path_matches = HashMap::<String, BTreeSet<String>>::new();
        let mut basename_path_matches = HashMap::<String, BTreeSet<String>>::new();
        let mut symbol_index_matches = HashMap::<String, BTreeSet<String>>::new();
        let mut graph_overlap_candidates = BTreeSet::<String>::new();
        if query_tokens.is_empty() {
            for cache_key in self.recall_docs.keys() {
                candidate_scores.insert(cache_key.clone(), 0.0);
            }
        } else {
            for token in &query_tokens {
                if let Some(postings) = self.recall_postings.get(token) {
                    let idf = bm25_idf(self.recall_docs.len(), postings.len());
                    for (cache_key, tf) in postings {
                        if let Some(doc) = self.recall_docs.get(cache_key) {
                            let score = bm25_score(
                                *tf as f64,
                                doc.doc_length as f64,
                                self.recall_avg_doc_length.max(1.0),
                                idf,
                            );
                            *candidate_scores.entry(cache_key.clone()).or_insert(0.0) += score;
                        }
                    }
                }
            }
        }

        for needle in options
            .path_filters
            .iter()
            .chain(query_path_terms.iter())
            .cloned()
        {
            let normalized_needle = needle.to_ascii_lowercase();
            if let Some(path_ref) = normalize_context_path(&needle, self.workspace_root.as_deref())
            {
                if let Some(cache_keys) = self.file_ref_index.get(&path_ref.canonical) {
                    for cache_key in cache_keys {
                        candidate_scores.entry(cache_key.clone()).or_insert(0.0);
                        canonical_path_matches
                            .entry(cache_key.clone())
                            .or_default()
                            .insert(path_ref.canonical.clone());
                        graph_overlap_candidates.insert(cache_key.clone());
                    }
                } else if let Some(basename) = path_ref.basename.as_ref() {
                    if let Some(canonicals) = self.basename_alias_index.get(basename) {
                        if canonicals.len() == 1 {
                            for canonical in canonicals {
                                if let Some(cache_keys) = self.file_ref_index.get(canonical) {
                                    for cache_key in cache_keys {
                                        candidate_scores.entry(cache_key.clone()).or_insert(0.0);
                                        basename_path_matches
                                            .entry(cache_key.clone())
                                            .or_default()
                                            .insert(canonical.clone());
                                        graph_overlap_candidates.insert(cache_key.clone());
                                    }
                                }
                            }
                        }
                    }
                }
            }
            for (canonical, cache_keys) in &self.file_ref_index {
                if canonical.contains(&normalized_needle)
                    || canonical.starts_with(&normalized_needle)
                {
                    for cache_key in cache_keys {
                        candidate_scores.entry(cache_key.clone()).or_insert(0.0);
                        canonical_path_matches
                            .entry(cache_key.clone())
                            .or_default()
                            .insert(canonical.clone());
                        graph_overlap_candidates.insert(cache_key.clone());
                    }
                }
            }
            for (basename, canonicals) in &self.basename_alias_index {
                if (basename.contains(&normalized_needle)
                    || basename.starts_with(&normalized_needle))
                    && canonicals.len() == 1
                {
                    for canonical in canonicals {
                        if let Some(cache_keys) = self.file_ref_index.get(canonical) {
                            for cache_key in cache_keys {
                                candidate_scores.entry(cache_key.clone()).or_insert(0.0);
                                basename_path_matches
                                    .entry(cache_key.clone())
                                    .or_default()
                                    .insert(canonical.clone());
                                graph_overlap_candidates.insert(cache_key.clone());
                            }
                        }
                    }
                }
            }
        }

        for needle in options
            .symbol_filters
            .iter()
            .map(|item| item.to_ascii_lowercase())
            .chain(query_tokens.iter().cloned())
        {
            for (symbol, cache_keys) in &self.symbol_index {
                if symbol == &needle || symbol.starts_with(&needle) || symbol.contains(&needle) {
                    for cache_key in cache_keys {
                        candidate_scores.entry(cache_key.clone()).or_insert(0.0);
                        symbol_index_matches
                            .entry(cache_key.clone())
                            .or_default()
                            .insert(symbol.clone());
                        graph_overlap_candidates.insert(cache_key.clone());
                    }
                }
            }
        }

        for needle in &query_tokens {
            for (test_id, cache_keys) in &self.test_index {
                if test_id == needle || test_id.starts_with(needle) || test_id.contains(needle) {
                    for cache_key in cache_keys {
                        candidate_scores.entry(cache_key.clone()).or_insert(0.0);
                        graph_overlap_candidates.insert(cache_key.clone());
                    }
                }
            }
        }

        let mut hits = Vec::new();
        for (cache_key, base_score) in candidate_scores {
            let Some(doc) = self.recall_docs.get(&cache_key) else {
                continue;
            };
            if let Some(target) = target_filter.as_ref() {
                if !doc.target.to_ascii_lowercase().contains(target) {
                    continue;
                }
            }
            if let Some(since) = options.since_unix {
                if doc.created_at_unix < since {
                    continue;
                }
            }
            if let Some(until) = options.until_unix {
                if doc.created_at_unix > until {
                    continue;
                }
            }
            if !packet_type_filters.is_empty()
                && !packet_type_filters.iter().all(|needle| {
                    doc.packet_types
                        .iter()
                        .any(|packet_type| packet_type.contains(needle))
                })
            {
                continue;
            }
            match options.scope {
                RecallScope::Global => {}
                RecallScope::TaskFirst => {
                    if let Some(task_id) = task_filter.as_ref() {
                        if !doc.task_ids.iter().any(|item| item == task_id) {
                            // allowed, but task-local docs receive a boost below
                        }
                    }
                }
                RecallScope::TaskOnly => {
                    if let Some(task_id) = task_filter.as_ref() {
                        if !doc.task_ids.iter().any(|item| item == task_id) {
                            continue;
                        }
                    } else {
                        continue;
                    }
                }
            }

            let age_secs = now.saturating_sub(doc.created_at_unix);
            let mut matched_paths = canonical_path_matches
                .remove(&cache_key)
                .unwrap_or_default()
                .into_iter()
                .collect::<Vec<_>>();
            let basename_matches = basename_path_matches
                .remove(&cache_key)
                .unwrap_or_default()
                .into_iter()
                .collect::<Vec<_>>();
            let extra_path_matches = basename_matches
                .iter()
                .filter(|item| !matched_paths.iter().any(|existing| existing == *item))
                .cloned()
                .collect::<Vec<_>>();
            matched_paths.extend(extra_path_matches);
            let mut matched_symbols = symbol_index_matches
                .remove(&cache_key)
                .unwrap_or_default()
                .into_iter()
                .collect::<Vec<_>>();
            for item in collect_matches(&doc.symbols, &symbol_filters, &query_tokens) {
                if !matched_symbols.iter().any(|existing| existing == &item) {
                    matched_symbols.push(item);
                }
            }
            let matched_tokens = query_tokens
                .iter()
                .filter(|token| doc.terms.contains_key(*token))
                .cloned()
                .collect::<Vec<_>>();

            let mut score = base_score;
            if !matched_paths.is_empty() {
                score += 2.0;
            }
            if !matched_symbols.is_empty() {
                score += 1.5;
            }
            if !path_filters.is_empty() && matched_paths.is_empty() {
                continue;
            }
            if !symbol_filters.is_empty() && matched_symbols.is_empty() {
                continue;
            }
            if let Some(task_id) = task_filter.as_ref() {
                if doc.task_ids.iter().any(|item| item == task_id) {
                    score += match options.scope {
                        RecallScope::Global => 0.35,
                        RecallScope::TaskFirst | RecallScope::TaskOnly => 1.0,
                    };
                }
            }
            score += (1.0_f64 / (1.0_f64 + (age_secs as f64 / 86_400.0_f64))).min(1.0_f64)
                * 0.25;

            if score <= 0.0
                || (query_tokens.is_empty()
                    && matched_paths.is_empty()
                    && matched_symbols.is_empty())
            {
                continue;
            }

            let mut match_reasons = Vec::new();
            if !matched_tokens.is_empty() {
                match_reasons.push("bm25_text".to_string());
            }
            if !matched_paths.is_empty() {
                if basename_matches.is_empty() {
                    match_reasons.push("canonical_path_match".to_string());
                } else {
                    match_reasons.push("basename_fallback".to_string());
                }
            }
            if !matched_symbols.is_empty() {
                match_reasons.push("symbol_match".to_string());
            }
            if graph_overlap_candidates.contains(&cache_key)
                && (matched_tokens.is_empty()
                    || !match_reasons.iter().any(|reason| reason == "bm25_text"))
            {
                match_reasons.push("graph_overlap".to_string());
            }
            if let Some(task_id) = task_filter.as_ref() {
                if doc.task_ids.iter().any(|item| item == task_id) {
                    match_reasons.push("task_scope".to_string());
                }
            }

            hits.push(RecallHit {
                cache_key: doc.cache_key.clone(),
                target: doc.target.clone(),
                created_at_unix: doc.created_at_unix,
                age_secs,
                score,
                summary: doc.summary.clone(),
                snippet: doc.snippet.clone(),
                matched_tokens,
                matched_paths,
                matched_symbols,
                match_reasons,
                packet_types: doc.packet_types.clone(),
                task_ids: doc.task_ids.clone(),
                budget_estimate: doc.budget_estimate.clone(),
            });
        }

        hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| b.created_at_unix.cmp(&a.created_at_unix))
                .then_with(|| a.cache_key.cmp(&b.cache_key))
        });
        hits.into_iter().take(options.limit.max(1)).collect()
    }
}

pub(crate) fn task_match_allowed(cache_key: &str, task_keys: Option<&BTreeSet<String>>) -> bool {
    task_keys
        .map(|keys| keys.contains(cache_key))
        .unwrap_or(true)
}

pub(crate) fn bm25_idf(doc_count: usize, posting_count: usize) -> f64 {
    (((doc_count.saturating_sub(posting_count) as f64) + 0.5) / (posting_count as f64 + 0.5) + 1.0)
        .ln()
}

pub(crate) fn bm25_score(tf: f64, doc_length: f64, avg_doc_length: f64, idf: f64) -> f64 {
    let k1 = 1.5;
    let b = 0.75;
    let norm = 1.0 - b + b * (doc_length / avg_doc_length.max(1.0));
    idf * (tf * (k1 + 1.0)) / (tf + k1 * norm)
}

pub(crate) fn collect_matches(
    haystack: &[String],
    explicit_filters: &[String],
    query_tokens: &[String],
) -> Vec<String> {
    let mut matches = Vec::new();
    let mut needles = explicit_filters.to_vec();
    for token in query_tokens {
        if !needles.iter().any(|existing| existing == token) {
            needles.push(token.clone());
        }
    }

    for item in haystack {
        let lower = item.to_ascii_lowercase();
        if needles
            .iter()
            .any(|needle| lower == *needle || lower.starts_with(needle) || lower.contains(needle))
            && !matches.iter().any(|existing| existing == item)
        {
            matches.push(item.clone());
        }
    }
    matches
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
