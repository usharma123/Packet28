use super::*;

impl PacketCache {
    pub fn related_entries(
        &self,
        task_id: Option<&str>,
        canonical_paths: &[String],
        symbols: &[String],
        tests: &[String],
    ) -> Vec<RelatedEntryMatch> {
        let task_filter = task_id.map(|value| value.to_ascii_lowercase());
        let task_keys = match task_filter.as_ref() {
            Some(task_id) => match self.task_index.get(task_id) {
                Some(keys) => Some(keys.clone()),
                None => return Vec::new(),
            },
            None => None,
        };
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
        let effective_mode = resolve_recall_mode(query, options);
        let newest_curated_by_dedupe = self
            .recall_docs
            .values()
            .filter_map(|doc| {
                (doc.source_tier == MemorySourceTier::CuratedMemory).then_some((
                    doc.dedupe_key.as_ref()?,
                    (doc.created_at_unix, &doc.cache_key),
                ))
            })
            .fold(
                HashMap::<String, (u64, String)>::new(),
                |mut acc, (dedupe_key, (created_at_unix, cache_key))| {
                    let update = acc
                        .get(dedupe_key)
                        .map(|(seen_created_at, seen_key)| {
                            created_at_unix > *seen_created_at
                                || (created_at_unix == *seen_created_at && cache_key > seen_key)
                        })
                        .unwrap_or(true);
                    if update {
                        acc.insert(dedupe_key.clone(), (created_at_unix, cache_key.clone()));
                    }
                    acc
                },
            );
        for (cache_key, base_score) in candidate_scores {
            let Some(doc) = self.recall_docs.get(&cache_key) else {
                continue;
            };
            let dedupe_superseded = doc
                .dedupe_key
                .as_ref()
                .and_then(|dedupe_key| newest_curated_by_dedupe.get(dedupe_key))
                .is_some_and(|(_, newest_key)| newest_key != &doc.cache_key);
            if (doc.superseded || dedupe_superseded) && !options.include_debug {
                continue;
            }
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
            let matched_packet_type = !packet_type_filters.is_empty();
            let explicit_filter_match = !matched_paths.is_empty()
                || !matched_symbols.is_empty()
                || matched_packet_type
                || task_filter
                    .as_ref()
                    .is_some_and(|task_id| doc.task_ids.iter().any(|item| item == task_id));

            let mut score = base_score;
            let task_scoped_match = task_filter
                .as_ref()
                .is_some_and(|task_id| doc.task_ids.iter().any(|item| item == task_id));
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
            if task_scoped_match {
                score += match options.scope {
                    RecallScope::Global => 0.35,
                    RecallScope::TaskFirst | RecallScope::TaskOnly => 1.0,
                };
            }
            score += recall_source_tier_score(
                doc.source_tier,
                effective_mode,
                !matched_paths.is_empty(),
                !matched_symbols.is_empty(),
                task_scoped_match,
                explicit_filter_match,
            );
            score += (1.0_f64 / (1.0_f64 + (age_secs as f64 / 86_400.0_f64))).min(1.0_f64) * 0.25;

            if score <= 0.0
                || (query_tokens.is_empty()
                    && matched_paths.is_empty()
                    && matched_symbols.is_empty()
                    && !matched_packet_type)
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
            if matched_packet_type {
                match_reasons.push("packet_type_filter".to_string());
            }
            if graph_overlap_candidates.contains(&cache_key)
                && (matched_tokens.is_empty()
                    || !match_reasons.iter().any(|reason| reason == "bm25_text"))
            {
                match_reasons.push("graph_overlap".to_string());
            }
            if task_scoped_match {
                match_reasons.push("task_scope".to_string());
            }
            match effective_mode {
                RecallMode::Auto => match_reasons.push("mode_auto".to_string()),
                RecallMode::Conceptual => match_reasons.push("mode_conceptual".to_string()),
                RecallMode::Telemetry => match_reasons.push("mode_telemetry".to_string()),
            }
            match doc.source_tier {
                MemorySourceTier::CuratedMemory => {
                    match_reasons.push("curated_memory".to_string());
                }
                MemorySourceTier::Telemetry => {
                    match_reasons.push("telemetry".to_string());
                }
                MemorySourceTier::Standard => {}
            }
            if doc.superseded || dedupe_superseded {
                match_reasons.push("superseded".to_string());
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
                source_tier: doc.source_tier,
                memory_kind: doc.memory_kind,
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

fn query_explicitly_targets_telemetry(query: &str, options: &RecallOptions) -> bool {
    options
        .target
        .as_ref()
        .is_some_and(|target| target.to_ascii_lowercase().contains("agenty.state"))
        || options
            .packet_types
            .iter()
            .any(|packet_type| packet_type.to_ascii_lowercase().contains("agent_state"))
        || tokenize(query).iter().any(|token| {
            matches!(
                token.as_str(),
                "agent"
                    | "state"
                    | "event"
                    | "events"
                    | "tool"
                    | "tools"
                    | "invocation"
                    | "invocations"
                    | "focus"
                    | "telemetry"
            )
        })
}

fn resolve_recall_mode(query: &str, options: &RecallOptions) -> RecallMode {
    match options.mode {
        RecallMode::Auto => {
            if query_explicitly_targets_telemetry(query, options) {
                RecallMode::Telemetry
            } else {
                RecallMode::Conceptual
            }
        }
        mode => mode,
    }
}

fn recall_source_tier_score(
    source_tier: RecallSourceTier,
    mode: RecallMode,
    has_path_match: bool,
    has_symbol_match: bool,
    task_scoped_match: bool,
    explicit_filter_match: bool,
) -> f64 {
    match mode {
        RecallMode::Auto | RecallMode::Conceptual => match source_tier {
            MemorySourceTier::CuratedMemory => 3.0,
            MemorySourceTier::Telemetry => {
                let mut score = if explicit_filter_match { -1.25 } else { -4.25 };
                if has_path_match || has_symbol_match {
                    score += 1.0;
                }
                if task_scoped_match {
                    score += 0.5;
                }
                score
            }
            MemorySourceTier::Standard => 0.0,
        },
        RecallMode::Telemetry => match source_tier {
            MemorySourceTier::CuratedMemory => 0.5,
            MemorySourceTier::Telemetry => {
                let mut score = 3.0;
                if has_path_match || has_symbol_match {
                    score += 1.0;
                }
                if task_scoped_match {
                    score += 0.5;
                }
                score
            }
            MemorySourceTier::Standard => 0.0,
        },
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_task_filter_does_not_fall_back_to_global_related_entries() {
        let mut cache = PacketCache::new();
        let mut hooks = NoopDeltaReuseHooks;
        let lookup = cache.lookup_with_hooks(
            "demo.reducer",
            &serde_json::json!({"task_id":"known-task"}),
            &mut hooks,
        );
        cache.put_with_hooks(
            "demo.reducer",
            &lookup,
            vec![CachePacket {
                body: serde_json::json!({
                    "task_id": "known-task",
                    "files": [{"path": "src/main.rs"}]
                }),
                ..CachePacket::default()
            }],
            Value::Null,
            &mut hooks,
        );

        let matches =
            cache.related_entries(Some("missing-task"), &["src/main.rs".to_string()], &[], &[]);

        assert!(matches.is_empty());
    }

    #[test]
    fn packet_type_only_queries_can_return_hits() {
        let mut cache = PacketCache::new();
        let mut hooks = NoopDeltaReuseHooks;
        let lookup = cache.lookup_with_hooks(
            "demo.reducer",
            &serde_json::json!({"task":"pt"}),
            &mut hooks,
        );
        cache.put_with_hooks(
            "demo.reducer",
            &lookup,
            vec![CachePacket {
                body: serde_json::json!({
                    "packet_type": "suite.context.manage.v1",
                    "summary": "context packet"
                }),
                ..CachePacket::default()
            }],
            Value::Null,
            &mut hooks,
        );

        let hits = cache.recall(
            "",
            &RecallOptions {
                packet_types: vec!["context.manage".to_string()],
                ..RecallOptions::default()
            },
        );

        assert_eq!(hits.len(), 1);
        assert!(hits[0]
            .match_reasons
            .iter()
            .any(|reason| reason == "packet_type_filter"));
    }

    #[test]
    fn curated_memory_outranks_telemetry_for_conceptual_queries() {
        let mut cache = PacketCache::new();
        let mut hooks = NoopDeltaReuseHooks;

        let telemetry_lookup = cache.lookup_with_hooks(
            "agenty.state.write",
            &serde_json::json!({"task_id":"task-conceptual"}),
            &mut hooks,
        );
        cache.put_with_hooks(
            "agenty.state.write",
            &telemetry_lookup,
            vec![CachePacket {
                body: serde_json::json!({
                    "packet_type": "suite.agent.state.v1",
                    "summary": "tool invocation completed task=task-conceptual tool=Read kind=Read broker context lookup",
                    "task_id": "task-conceptual",
                }),
                ..CachePacket::default()
            }],
            Value::Null,
            &mut hooks,
        );

        let curated_lookup = cache.lookup_with_hooks(
            "packet28.broker_memory.write",
            &serde_json::json!({"task_id":"task-conceptual"}),
            &mut hooks,
        );
        let curated = cache.put_with_hooks(
            "packet28.broker_memory.write",
            &curated_lookup,
            vec![CachePacket {
                body: serde_json::json!({
                    "packet_type": "suite.packet28.broker_memory.v1",
                    "summary": "Checkpoint handoff for task-conceptual: inspect the broker context and resume from the latest brief",
                    "task_id": "task-conceptual",
                }),
                ..CachePacket::default()
            }],
            Value::Null,
            &mut hooks,
        );

        let hits = cache.recall(
            "broker context handoff resume latest brief",
            &RecallOptions::default(),
        );

        assert!(!hits.is_empty());
        assert_eq!(hits[0].cache_key, curated.cache_key);
        assert_eq!(hits[0].source_tier, RecallSourceTier::CuratedMemory);
        if let Some(telemetry_index) = hits
            .iter()
            .position(|hit| hit.source_tier == RecallSourceTier::Telemetry)
        {
            assert!(telemetry_index > 0);
        }
    }

    #[test]
    fn telemetry_remains_searchable_for_explicit_task_activity_queries() {
        let mut cache = PacketCache::new();
        let mut hooks = NoopDeltaReuseHooks;
        let lookup = cache.lookup_with_hooks(
            "agenty.state.write",
            &serde_json::json!({"task_id":"task-telemetry"}),
            &mut hooks,
        );
        let stored = cache.put_with_hooks(
            "agenty.state.write",
            &lookup,
            vec![CachePacket {
                body: serde_json::json!({
                    "packet_type": "suite.agent.state.v1",
                    "summary": "tool invocation completed task=task-telemetry seq=7 tool=Read kind=Read",
                    "task_id": "task-telemetry",
                    "files": [{"path":"src/main.rs"}],
                    "symbols": [{"name":"parse_input"}]
                }),
                ..CachePacket::default()
            }],
            Value::Null,
            &mut hooks,
        );

        let hits = cache.recall(
            "task telemetry tool invocation read src/main.rs parse_input",
            &RecallOptions {
                task_id: Some("task-telemetry".to_string()),
                scope: RecallScope::TaskFirst,
                path_filters: vec!["src/main.rs".to_string()],
                symbol_filters: vec!["parse_input".to_string()],
                ..RecallOptions::default()
            },
        );

        assert!(!hits.is_empty());
        assert_eq!(hits[0].cache_key, stored.cache_key);
        assert_eq!(hits[0].source_tier, RecallSourceTier::Telemetry);
    }
}
