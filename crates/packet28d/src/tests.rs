use super::*;

#[test]
fn explicit_limits_override_verbosity_alias() {
    let mut section_limits = BTreeMap::new();
    section_limits.insert("relevant_context".to_string(), 2);
    let limits = resolve_effective_limits(
        BrokerAction::Plan,
        Some(BrokerVerbosity::Rich),
        Some(3),
        Some(5),
        &section_limits,
    );
    assert_eq!(limits.max_sections, 3);
    assert_eq!(limits.default_max_items_per_section, 5);
    assert_eq!(limits.section_item_limits["relevant_context"], 2);
}

#[test]
fn omitted_explicit_limits_use_deterministic_action_defaults() {
    let plan_limits =
        resolve_effective_limits(BrokerAction::Plan, None, None, None, &BTreeMap::new());
    let choose_tool_limits =
        resolve_effective_limits(BrokerAction::ChooseTool, None, None, None, &BTreeMap::new());
    assert_eq!(plan_limits.max_sections, 8);
    assert_eq!(plan_limits.default_max_items_per_section, 8);
    assert_eq!(plan_limits.section_item_limits["code_evidence"], 6);
    assert_eq!(choose_tool_limits.max_sections, 6);
    assert_eq!(choose_tool_limits.default_max_items_per_section, 5);
}

#[test]
fn brief_always_starts_with_supersession_header() {
    let brief = render_brief(
        "task-123",
        "7",
        &[BrokerSection {
            id: "task_objective".to_string(),
            title: "Task Objective".to_string(),
            body: "Investigate auth flow".to_string(),
            priority: 1,
            source_kind: BrokerSourceKind::Derived,
        }],
    );
    assert!(brief.starts_with("[Packet28 Context v7"));
    assert!(brief.contains("supersedes all prior Packet28 context"));
}

#[test]
fn normalize_plan_steps_trims_and_assigns_missing_ids() {
    let normalized = normalize_plan_steps(&[BrokerPlanStep {
        id: " ".to_string(),
        action: " Edit ".to_string(),
        description: Some(" touch auth ".to_string()),
        paths: vec!["src/auth.rs".to_string(), "src/auth.rs".to_string()],
        symbols: vec![" Login ".to_string()],
        depends_on: vec![" prev ".to_string(), "prev".to_string()],
    }]);
    assert_eq!(normalized[0].id, "step-1");
    assert_eq!(normalized[0].action, "edit");
    assert_eq!(normalized[0].description.as_deref(), Some("touch auth"));
    assert_eq!(normalized[0].paths, vec!["src/auth.rs".to_string()]);
    assert_eq!(normalized[0].symbols, vec!["Login".to_string()]);
    assert_eq!(normalized[0].depends_on, vec!["prev".to_string()]);
}

#[test]
fn infer_scope_paths_prefers_explicit_paths() {
    let inferred = infer_scope_paths(
        "refactor auth module",
        &mapy_core::RepoMapPayloadRich {
            files_ranked: vec![
                mapy_core::RankedFileRich {
                    path: "src/auth.rs".to_string(),
                    score: 1.0,
                    symbol_count: 1,
                    import_count: 0,
                },
                mapy_core::RankedFileRich {
                    path: "src/session.rs".to_string(),
                    score: 0.8,
                    symbol_count: 1,
                    import_count: 0,
                },
            ],
            ..Default::default()
        },
        &["src/session.rs".to_string()],
        &[],
    );
    assert_eq!(inferred, vec!["src/session.rs".to_string()]);
}

#[test]
fn derive_query_focus_extracts_symbol_terms() {
    let focus = derive_query_focus(Some(
        "What does StringUtils.abbreviate() do in src/main/java/StringUtils.java?",
    ));
    assert!(focus
        .full_symbol_terms
        .contains(&"StringUtils.abbreviate".to_string()));
    assert!(focus.symbol_terms.iter().any(|item| item == "StringUtils"));
    assert!(focus.symbol_terms.iter().any(|item| item == "abbreviate"));
    assert!(focus
        .path_terms
        .iter()
        .any(|item| item.contains("StringUtils.java")));
}

#[test]
fn derive_query_focus_filters_stopwords_but_keeps_symbols() {
    let focus = derive_query_focus(Some(
        "Where is StringUtils.isBlank defined and used across the codebase?",
    ));
    assert!(!focus.text_tokens.iter().any(|item| item == "where"));
    assert!(!focus.text_tokens.iter().any(|item| item == "defined"));
    assert!(!focus.text_tokens.iter().any(|item| item == "used"));
    assert!(focus
        .full_symbol_terms
        .contains(&"StringUtils.isBlank".to_string()));
    assert!(focus
        .symbol_terms
        .iter()
        .any(|item| item.eq_ignore_ascii_case("isBlank")));
}

#[test]
fn expand_scope_paths_pulls_adjacent_role_files() {
    let expanded = expand_scope_paths(
        "explain what diffy does",
        &mapy_core::RepoMapPayloadRich {
            files_ranked: vec![
                mapy_core::RankedFileRich {
                    path: "crates/diffy-core/src/lib.rs".to_string(),
                    score: 1.0,
                    symbol_count: 2,
                    import_count: 1,
                },
                mapy_core::RankedFileRich {
                    path: "crates/diffy-core/src/report.rs".to_string(),
                    score: 0.7,
                    symbol_count: 2,
                    import_count: 0,
                },
                mapy_core::RankedFileRich {
                    path: "crates/diffy-cli/src/cmd_analyze.rs".to_string(),
                    score: 0.65,
                    symbol_count: 2,
                    import_count: 1,
                },
                mapy_core::RankedFileRich {
                    path: "crates/testy-core/src/lib.rs".to_string(),
                    score: 0.6,
                    symbol_count: 2,
                    import_count: 0,
                },
            ],
            symbols_ranked: vec![
                mapy_core::RankedSymbolRich {
                    name: "analyze".to_string(),
                    file: "crates/diffy-cli/src/cmd_analyze.rs".to_string(),
                    kind: "function".to_string(),
                    score: 0.9,
                },
                mapy_core::RankedSymbolRich {
                    name: "render_report".to_string(),
                    file: "crates/diffy-core/src/report.rs".to_string(),
                    kind: "function".to_string(),
                    score: 0.8,
                },
            ],
            edges: vec![
                mapy_core::RepoEdgeRich {
                    from: "crates/diffy-cli/src/cmd_analyze.rs".to_string(),
                    to: "crates/diffy-core/src/lib.rs".to_string(),
                    kind: "import".to_string(),
                },
                mapy_core::RepoEdgeRich {
                    from: "crates/diffy-core/src/report.rs".to_string(),
                    to: "crates/diffy-core/src/lib.rs".to_string(),
                    kind: "import".to_string(),
                },
            ],
            ..Default::default()
        },
        &["crates/diffy-core/src/lib.rs".to_string()],
        &["diffy".to_string()],
        6,
    );
    assert!(expanded.contains(&"crates/diffy-core/src/report.rs".to_string()));
    assert!(expanded.contains(&"crates/diffy-cli/src/cmd_analyze.rs".to_string()));
}

fn write_search_fixture(root: &std::path::Path, files: &[(&str, &str)]) {
    for (relative_path, contents) in files {
        let path = root.join(relative_path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
}

fn run_search_execution_for_query(
    root: &std::path::Path,
    query: &str,
    action: BrokerAction,
) -> SearchExecution {
    let snapshot = suite_packet_core::AgentSnapshotPayload::default();
    let request = BrokerGetContextRequest {
        task_id: "task-search".to_string(),
        action: Some(action),
        query: Some(query.to_string()),
        ..BrokerGetContextRequest::default()
    };
    let query_focus = derive_query_focus(Some(query));
    build_reducer_search_execution(None, root, &snapshot, &request, &query_focus, action, 8, 8)
}

#[test]
fn exact_symbol_query_returns_definition_first_without_fallback() {
    let temp_dir = tempfile::tempdir().unwrap();
    let root = temp_dir.path();
    write_search_fixture(
        root,
        &[
            (
                "src/alpha.rs",
                "pub struct Alpha;\nimpl Alpha { pub fn build() {} }\n",
            ),
            (
                "src/mentions.rs",
                "fn helper() { let _ = Alpha::build(); }\n",
            ),
        ],
    );

    let execution =
        run_search_execution_for_query(root, "Where is Alpha defined?", BrokerAction::Inspect);
    assert!(!execution.used_fallback);
    assert_eq!(
        execution.files.first().map(|file| file.path.as_str()),
        Some("src/alpha.rs")
    );
    assert!(execution.files[0].definition_hits > 0);
}

#[test]
fn vague_query_triggers_fallback_only_after_weak_first_pass() {
    let temp_dir = tempfile::tempdir().unwrap();
    let root = temp_dir.path();
    write_search_fixture(
        root,
        &[
            ("src/alpha.rs", "pub struct AlphaService;\n"),
            (
                "src/alpha_update.rs",
                "pub fn update_state_for_alpha_service() {}\n",
            ),
        ],
    );

    let execution = run_search_execution_for_query(
        root,
        "How is AlphaService.updateState updated?",
        BrokerAction::Inspect,
    );
    assert!(execution.used_fallback);
    assert!(execution
        .files
        .iter()
        .any(|file| file.path == "src/alpha_update.rs"));
    assert!(execution
        .evidence_by_file
        .get("src/alpha_update.rs")
        .is_some_and(|summary| summary
            .rendered_lines
            .iter()
            .any(|line| line.contains("update_state_for_alpha_service"))));
}

#[test]
fn definition_hits_outrank_bulk_references() {
    let root = std::env::temp_dir().join(format!(
        "packet28d-search-definition-rank-{}",
        std::process::id()
    ));
    write_search_fixture(
            &root,
            &[
                (
                    "src/alpha.rs",
                    "pub struct Alpha;\n",
                ),
                (
                    "src/references.rs",
                    "fn one() { let _ = Alpha; }\nfn two() { let _ = Alpha; }\nfn three() { let _ = Alpha; }\nfn four() { let _ = Alpha; }\n",
                ),
            ],
        );

    let execution = run_search_execution_for_query(&root, "Alpha", BrokerAction::Inspect);
    assert_eq!(
        execution.files.first().map(|file| file.path.as_str()),
        Some("src/alpha.rs")
    );
    assert!(execution.files[0].definition_hits >= execution.files[1].definition_hits);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn broad_generic_tokens_do_not_outrank_exact_symbol_hits() {
    let root = std::env::temp_dir().join(format!(
        "packet28d-search-generic-rank-{}",
        std::process::id()
    ));
    write_search_fixture(
            &root,
            &[
                (
                    "src/request.rs",
                    "pub struct BrokerWriteStateRequest {\n    pub task_id: String,\n}\n",
                ),
                (
                    "src/noise.rs",
                    "pub fn a(task_id: &str) {}\npub fn b(task_id: &str) {}\npub fn c(task_id: &str) {}\npub fn d(task_id: &str) {}\n",
                ),
            ],
        );

    let execution = run_search_execution_for_query(
        &root,
        "How does BrokerWriteStateRequest use task_id?",
        BrokerAction::Inspect,
    );
    assert_eq!(
        execution.files.first().map(|file| file.path.as_str()),
        Some("src/request.rs")
    );
    assert!(execution.files[0].exact_symbol_hits > 0);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn choose_tool_uses_the_same_staged_search_planner() {
    let root = std::env::temp_dir().join(format!(
        "packet28d-search-choose-tool-{}",
        std::process::id()
    ));
    write_search_fixture(
        &root,
        &[
            ("src/alpha.rs", "pub struct AlphaService;\n"),
            (
                "src/alpha_update.rs",
                "pub fn update_state_for_alpha_service() {}\n",
            ),
        ],
    );

    let execution = run_search_execution_for_query(
        &root,
        "How is AlphaService.updateState updated?",
        BrokerAction::ChooseTool,
    );
    assert!(execution.used_fallback);
    assert!(execution
        .files
        .iter()
        .any(|file| file.path == "src/alpha_update.rs"));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn extract_code_evidence_prefers_query_hits_and_context() {
    let root = std::env::temp_dir().join(format!("packet28d-code-evidence-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let path = root.join("src/lib.rs");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
            &path,
            "use std::fmt;\n\npub struct Diffy;\nimpl Diffy {\n    pub fn analyze() {}\n    pub fn summarize() {}\n}\n",
        )
        .unwrap();

    let evidence = extract_code_evidence(
        &root,
        "src/lib.rs",
        &derive_query_focus(Some("Diffy.analyze")),
        &[],
        3,
        6,
    );
    assert!(evidence
        .primary_match_symbol
        .as_deref()
        .is_some_and(|value| value == "analyze" || value == "Diffy"));
    assert!(evidence
        .rendered_lines
        .iter()
        .any(|line| line.contains("pub fn analyze")));
    assert!(evidence
        .rendered_lines
        .iter()
        .all(|line| !line.contains("use std::fmt")));
    assert!(evidence
        .rendered_lines
        .iter()
        .any(|line| line.contains("impl Diffy") || line.contains("pub struct Diffy")));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn extract_code_evidence_ignores_license_headers_and_prefers_focus_symbols() {
    let root = std::env::temp_dir().join(format!(
        "packet28d-code-evidence-java-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let path = root.join("src/StringUtils.java");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
            &path,
            "/*\n * Licensed to the Apache Software Foundation (ASF)\n */\npackage org.example;\n\npublic class StringUtils {\n    public static boolean isBlank(final CharSequence cs) {\n        return cs == null || cs.length() == 0;\n    }\n}\n",
        )
        .unwrap();

    let mut focus = derive_query_focus(Some(
        "Where is StringUtils.isBlank defined and used across the codebase?",
    ));
    focus.full_symbol_terms.clear();
    focus.symbol_terms.clear();
    let focus = merge_query_focus_with_symbols(focus, &["isBlank".to_string()]);
    let evidence = extract_code_evidence(&root, "src/StringUtils.java", &focus, &[], 3, 6);
    assert!(evidence
        .rendered_lines
        .iter()
        .any(|line| line.contains("isBlank(final CharSequence cs)")));
    assert!(evidence
        .rendered_lines
        .iter()
        .all(|line| !line.contains("Licensed to the Apache")));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn extract_code_evidence_prefers_symbol_definitions_over_comment_mentions() {
    let root = std::env::temp_dir().join(format!(
        "packet28d-code-evidence-priority-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let path = root.join("src/StringUtils.java");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
            &path,
            "package org.example;\n\npublic final class StringUtils {\n    /** Mention isBlank in docs before the definition. */\n    public static boolean isBlank(final CharSequence cs) {\n        return cs == null || cs.length() == 0;\n    }\n}\n",
        )
        .unwrap();

    let mut focus = derive_query_focus(Some(
        "Where is StringUtils.isBlank defined and used across the codebase?",
    ));
    focus.full_symbol_terms.clear();
    focus.symbol_terms.clear();
    let focus = merge_query_focus_with_symbols(focus, &["isBlank".to_string()]);
    let evidence = extract_code_evidence(&root, "src/StringUtils.java", &focus, &[], 1, 3);
    assert!(evidence
        .rendered_lines
        .iter()
        .any(|line| line.contains("isBlank(final CharSequence cs)")));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn extract_code_evidence_prefers_region_hints_when_present() {
    let root = std::env::temp_dir().join(format!(
        "packet28d-code-evidence-region-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let path = root.join("src/StringUtils.java");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
            &path,
            "package org.example;\n\npublic final class StringUtils {\n    /** Mention isBlank in docs before the definition. */\n    public static String describe() { return \"isBlank docs\"; }\n\n    public static boolean isBlank(final CharSequence cs) {\n        return cs == null || cs.length() == 0;\n    }\n}\n",
        )
        .unwrap();

    let mut focus = derive_query_focus(Some(
        "Where is StringUtils.isBlank defined and used across the codebase?",
    ));
    focus.full_symbol_terms.clear();
    focus.symbol_terms.clear();
    let focus = merge_query_focus_with_symbols(focus, &["isBlank".to_string()]);
    let provenance = vec![ToolResultProvenance {
        regions: vec!["src/StringUtils.java:7-8".to_string()],
    }];
    let evidence = extract_code_evidence(&root, "src/StringUtils.java", &focus, &provenance, 1, 3);
    assert!(evidence.from_region_hint);
    assert_eq!(
        evidence.primary_match_kind,
        Some(EvidenceMatchKind::DefinesSymbol)
    );
    assert!(evidence
        .rendered_lines
        .iter()
        .any(|line| line.contains("isBlank(final CharSequence cs)")));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn extract_code_evidence_skips_unrelated_signatures_when_symbol_focus_exists() {
    let root = std::env::temp_dir().join(format!(
        "packet28d-code-evidence-unrelated-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let path = root.join("src/ArrayUtils.java");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
            &path,
            "package org.example;\n\npublic class ArrayUtils {\n    public static void shuffle() {}\n}\n",
        )
        .unwrap();

    let mut focus = derive_query_focus(Some(
        "Where is StringUtils.isBlank defined and used across the codebase?",
    ));
    focus.full_symbol_terms.clear();
    focus.symbol_terms.clear();
    let focus = merge_query_focus_with_symbols(focus, &["isBlank".to_string()]);
    let evidence = extract_code_evidence(&root, "src/ArrayUtils.java", &focus, &[], 3, 6);
    assert!(evidence.rendered_lines.is_empty());
    assert!(evidence.primary_match_symbol.is_none());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn extract_code_evidence_prefers_method_match_over_class_declaration() {
    let root = std::env::temp_dir().join(format!(
        "packet28d-code-evidence-method-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let path = root.join("src/ArrayUtils.java");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
            &path,
            "package org.example;\n\npublic class ArrayUtils {\n    public static void shuffle() {}\n}\n",
        )
        .unwrap();

    let mut focus = derive_query_focus(Some(
        "Add deterministic seeded shuffle overloads to ArrayUtils",
    ));
    focus.full_symbol_terms.clear();
    focus.symbol_terms.clear();
    let focus =
        merge_query_focus_with_symbols(focus, &["ArrayUtils".to_string(), "shuffle".to_string()]);
    let evidence = extract_code_evidence(&root, "src/ArrayUtils.java", &focus, &[], 3, 6);
    assert!(evidence
        .rendered_lines
        .iter()
        .any(|line| line.contains("public static void shuffle")));
    assert!(evidence
        .rendered_lines
        .iter()
        .all(|line| !line.contains("public class ArrayUtils")));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn build_budget_notes_section_is_empty_without_budget_pruning() {
    let limits =
        resolve_effective_limits(BrokerAction::Inspect, None, None, None, &BTreeMap::new());
    assert!(build_budget_notes_section(&[], &limits).is_none());
    assert!(build_budget_notes_section(
        &[BrokerEvictionCandidate {
            section_id: "search_evidence".to_string(),
            reason: "search evidence can be regenerated".to_string(),
            est_tokens: 12,
        }],
        &limits
    )
    .is_none());
}

#[test]
fn postprocess_selected_sections_adds_budget_notes_and_compacts_tool_activity() {
    let limits =
        resolve_effective_limits(BrokerAction::Inspect, None, None, None, &BTreeMap::new());
    let snapshot = suite_packet_core::AgentSnapshotPayload {
        recent_tool_invocations: vec![suite_packet_core::ToolInvocationSummary {
            invocation_id: "tool-1".to_string(),
            sequence: 7,
            tool_name: "grep".to_string(),
            operation_kind: suite_packet_core::ToolOperationKind::Search,
            request_summary: Some("search for isBlank".to_string()),
            result_summary: Some("Validate.java:806 calls isBlank".to_string()),
            paths: vec!["src/Validate.java".to_string()],
            regions: vec!["src/Validate.java:806-806".to_string()],
            symbols: vec!["isBlank".to_string()],
            duration_ms: Some(12),
            ..Default::default()
        }],
        ..Default::default()
    };
    let sections = vec![
        BrokerSection {
            id: "task_objective".to_string(),
            title: "Task Objective".to_string(),
            body: "Where is StringUtils.isBlank defined and used?".to_string(),
            priority: 1,
            source_kind: BrokerSourceKind::Derived,
        },
        BrokerSection {
            id: "recent_tool_activity".to_string(),
            title: "Recent Tool Activity".to_string(),
            body: "- #7 grep [search] search for isBlank -> Validate.java:806 calls isBlank"
                .to_string(),
            priority: 1,
            source_kind: BrokerSourceKind::Derived,
        },
        BrokerSection {
            id: "code_evidence".to_string(),
            title: "Code Evidence".to_string(),
            body: "- src/Validate.java:806 if (StringUtils.isBlank(chars))".to_string(),
            priority: 1,
            source_kind: BrokerSourceKind::Derived,
        },
    ];
    let pruned = vec![BrokerEvictionCandidate {
        section_id: "search_evidence".to_string(),
        reason: "budget_pruned".to_string(),
        est_tokens: 491,
    }];

    let processed = postprocess_selected_sections(sections, &pruned, &snapshot, &limits);
    let budget_notes = processed
        .iter()
        .find(|section| section.id == "budget_notes")
        .expect("budget notes should be inserted");
    assert!(budget_notes
        .body
        .contains("search_evidence omitted due to budget"));
    assert!(budget_notes.body.contains("491"));
    let tool_activity = processed
        .iter()
        .find(|section| section.id == "recent_tool_activity")
        .expect("tool activity should remain");
    assert!(tool_activity.body.contains("paths=1"));
    assert!(tool_activity.body.contains("regions=1"));
    assert!(tool_activity.body.contains("duration=12ms"));
    assert!(!tool_activity.body.contains("->"));
}

#[test]
fn budget_pruning_drops_optional_sections_before_critical_ones() {
    let sections = vec![
        BrokerSection {
            id: "task_objective".to_string(),
            title: "Task Objective".to_string(),
            body: "Investigate Alpha".to_string(),
            priority: 1,
            source_kind: BrokerSourceKind::Derived,
        },
        BrokerSection {
            id: "code_evidence".to_string(),
            title: "Code Evidence".to_string(),
            body: [
                "- src/alpha.rs:1 fn alpha() {}",
                "- src/alpha.rs:2 struct Alpha;",
            ]
            .join("\n"),
            priority: 1,
            source_kind: BrokerSourceKind::Derived,
        },
        BrokerSection {
            id: "search_evidence".to_string(),
            title: "Relevant Files".to_string(),
            body: "- src/alpha.rs:1 [matches=2] — direct reducer hit for Alpha".to_string(),
            priority: 1,
            source_kind: BrokerSourceKind::Derived,
        },
        BrokerSection {
            id: "recent_tool_activity".to_string(),
            title: "Recent Tool Activity".to_string(),
            body: [
                "- #1 read [read] alpha -> found Alpha",
                "- #2 grep [search] alpha -> found alpha()",
            ]
            .join("\n"),
            priority: 2,
            source_kind: BrokerSourceKind::Derived,
        },
    ];
    let rendered = render_brief("task-a", "v1", &sections[..3]);
    let (budget_tokens, budget_bytes) = estimate_text_cost(&rendered);
    let (selected, evicted) = prune_sections_for_budget(
        BrokerAction::Inspect,
        sections,
        budget_tokens + 2,
        budget_bytes + 8,
        8,
    );
    assert!(selected.iter().any(|section| section.id == "code_evidence"));
    assert!(selected
        .iter()
        .any(|section| section.id == "search_evidence"));
    assert!(!selected
        .iter()
        .any(|section| section.id == "recent_tool_activity"));
    assert!(evicted.iter().any(|candidate| {
        candidate.section_id == "recent_tool_activity" && candidate.reason == "budget_pruned"
    }));
}

#[test]
fn budget_pruning_shrinks_critical_sections_before_dropping_them() {
    let code_evidence_body = (1..=8)
        .map(|idx| format!("- src/alpha.rs:{idx} evidence line {idx}"))
        .collect::<Vec<_>>()
        .join("\n");
    let sections = vec![
        BrokerSection {
            id: "task_objective".to_string(),
            title: "Task Objective".to_string(),
            body: "Edit Alpha".to_string(),
            priority: 1,
            source_kind: BrokerSourceKind::Derived,
        },
        BrokerSection {
            id: "code_evidence".to_string(),
            title: "Code Evidence".to_string(),
            body: code_evidence_body.clone(),
            priority: 1,
            source_kind: BrokerSourceKind::Derived,
        },
        BrokerSection {
            id: "search_evidence".to_string(),
            title: "Relevant Files".to_string(),
            body: "- src/alpha.rs:1 [matches=2] — direct reducer hit for Alpha".to_string(),
            priority: 1,
            source_kind: BrokerSourceKind::Derived,
        },
    ];
    let partial_sections = vec![
        sections[0].clone(),
        BrokerSection {
            id: "code_evidence".to_string(),
            title: "Code Evidence".to_string(),
            body: code_evidence_body
                .lines()
                .take(3)
                .collect::<Vec<_>>()
                .join("\n"),
            priority: 1,
            source_kind: BrokerSourceKind::Derived,
        },
    ];
    let partial_brief = render_brief("task-a", "v1", &partial_sections);
    let (budget_tokens, budget_bytes) = estimate_text_cost(&partial_brief);
    let (selected, _) = prune_sections_for_budget(
        BrokerAction::Inspect,
        sections,
        budget_tokens + 2,
        budget_bytes + 8,
        8,
    );
    let code_evidence = selected
        .iter()
        .find(|section| section.id == "code_evidence")
        .expect("code_evidence should be retained");
    assert!(code_evidence.body.len() < code_evidence_body.len());
    assert!(code_evidence.body.contains("src/alpha.rs:1"));
}

#[test]
fn inherit_broker_request_defaults_reuses_previous_follow_up_shape() {
    let previous = BrokerGetContextRequest {
        task_id: "task-a".to_string(),
        action: Some(BrokerAction::Inspect),
        budget_tokens: Some(700),
        budget_bytes: Some(2800),
        focus_paths: vec!["src/alpha.rs".to_string()],
        focus_symbols: vec!["Alpha".to_string()],
        query: Some("Where is Alpha defined?".to_string()),
        include_sections: vec!["task_objective".to_string(), "code_evidence".to_string()],
        verbosity: Some(BrokerVerbosity::Rich),
        response_mode: Some(BrokerResponseMode::Delta),
        max_sections: Some(5),
        default_max_items_per_section: Some(3),
        section_item_limits: BTreeMap::from([("code_evidence".to_string(), 2)]),
        persist_artifacts: Some(true),
        ..BrokerGetContextRequest::default()
    };
    let mut current = BrokerGetContextRequest {
        task_id: "task-a".to_string(),
        ..BrokerGetContextRequest::default()
    };

    inherit_broker_request_defaults(&mut current, Some(&previous));

    assert_eq!(current.action, Some(BrokerAction::Inspect));
    assert_eq!(current.query.as_deref(), Some("Where is Alpha defined?"));
    assert_eq!(current.focus_paths, vec!["src/alpha.rs"]);
    assert_eq!(current.focus_symbols, vec!["Alpha"]);
    assert_eq!(
        current.include_sections,
        vec!["task_objective".to_string(), "code_evidence".to_string()]
    );
    assert_eq!(current.response_mode, Some(BrokerResponseMode::Delta));
    assert_eq!(current.section_item_limits["code_evidence"], 2);
}

#[test]
fn reducer_search_only_runs_when_evidence_sections_are_allowed() {
    let only_summary = HashSet::from(["task_objective".to_string(), "progress".to_string()]);
    assert!(!should_run_reducer_search(&only_summary));

    let with_search = HashSet::from(["search_evidence".to_string()]);
    assert!(should_run_reducer_search(&with_search));

    let with_code = HashSet::from(["code_evidence".to_string()]);
    assert!(should_run_reducer_search(&with_code));
}

#[test]
fn render_task_memory_lines_surfaces_recent_state() {
    let snapshot = suite_packet_core::AgentSnapshotPayload {
        files_read: vec!["src/alpha.rs".to_string()],
        latest_intention: Some(suite_packet_core::AgentIntention {
            text: "Inspect Alpha before editing".to_string(),
            note: Some("Need a clean handoff breadcrumb".to_string()),
            step_id: Some("investigating".to_string()),
            paths: vec!["src/alpha.rs".to_string()],
            occurred_at_unix: 1,
            ..suite_packet_core::AgentIntention::default()
        }),
        latest_checkpoint_id: Some("cp-1".to_string()),
        checkpoint_note: Some("Validated shuffle scope".to_string()),
        checkpoint_focus_paths: vec!["src/alpha.rs".to_string()],
        checkpoint_focus_symbols: vec!["Alpha".to_string()],
        changed_paths_since_checkpoint: vec!["src/beta.rs".to_string()],
        changed_symbols_since_checkpoint: vec!["Beta".to_string()],
        evidence_artifact_ids: vec!["artifact-1".to_string()],
        recent_tool_invocations: vec![suite_packet_core::ToolInvocationSummary {
            invocation_id: "tool-1".to_string(),
            sequence: 7,
            tool_name: "manual.read".to_string(),
            operation_kind: suite_packet_core::ToolOperationKind::Read,
            request_summary: Some("Read alpha".to_string()),
            result_summary: Some("Found Alpha".to_string()),
            paths: vec!["src/alpha.rs".to_string()],
            symbols: vec!["Alpha".to_string()],
            occurred_at_unix: 1,
            ..suite_packet_core::ToolInvocationSummary::default()
        }],
        ..suite_packet_core::AgentSnapshotPayload::default()
    };

    let rendered = render_task_memory_lines(&snapshot);

    assert!(rendered.iter().any(
        |line| line.contains("latest intention [investigating]: Inspect Alpha before editing")
    ));
    assert!(rendered
        .iter()
        .any(|line| line.contains("latest intention note: Need a clean handoff breadcrumb")));
    assert!(rendered
        .iter()
        .any(|line| line.contains("latest tool: manual.read")));
    assert!(rendered
        .iter()
        .any(|line| line.contains("recently read: src/alpha.rs")));
    assert!(rendered
        .iter()
        .any(|line| line.contains("latest checkpoint: cp-1")));
    assert!(rendered
        .iter()
        .any(|line| line.contains("checkpoint note: Validated shuffle scope")));
    assert!(rendered
        .iter()
        .any(|line| line.contains("checkpoint focus path: src/alpha.rs")));
    assert!(rendered
        .iter()
        .any(|line| line.contains("checkpoint focus symbol: Alpha")));
    assert!(rendered
        .iter()
        .any(|line| line.contains("changed since checkpoint: src/beta.rs")));
    assert!(rendered
        .iter()
        .any(|line| line.contains("changed symbol since checkpoint: Beta")));
    assert!(rendered
        .iter()
        .any(|line| line.contains("evidence artifact: artifact-1")));
}

#[test]
fn compute_handoff_state_requires_checkpoint_and_tracks_newer_intentions() {
    let empty_snapshot = suite_packet_core::AgentSnapshotPayload::default();
    let (ready_without_checkpoint, _) = compute_handoff_state(None, &empty_snapshot);
    assert!(!ready_without_checkpoint);

    let snapshot = suite_packet_core::AgentSnapshotPayload {
        latest_checkpoint_id: Some("cp-1".to_string()),
        latest_intention: Some(suite_packet_core::AgentIntention {
            text: "Resume editing beta".to_string(),
            occurred_at_unix: 20,
            ..suite_packet_core::AgentIntention::default()
        }),
        ..suite_packet_core::AgentSnapshotPayload::default()
    };
    let (ready_initial, _) = compute_handoff_state(None, &snapshot);
    assert!(ready_initial);

    let task = TaskRecord {
        task_id: "task-a".to_string(),
        latest_handoff_generated_at_unix: Some(10),
        latest_handoff_checkpoint_id: Some("cp-1".to_string()),
        ..TaskRecord::default()
    };
    let (ready_newer_intention, _) = compute_handoff_state(Some(&task), &snapshot);
    assert!(ready_newer_intention);

    let stale_snapshot = suite_packet_core::AgentSnapshotPayload {
        latest_checkpoint_id: Some("cp-1".to_string()),
        latest_intention: Some(suite_packet_core::AgentIntention {
            text: "Resume editing beta".to_string(),
            occurred_at_unix: 5,
            ..suite_packet_core::AgentIntention::default()
        }),
        ..suite_packet_core::AgentSnapshotPayload::default()
    };
    let (ready_stale, _) = compute_handoff_state(Some(&task), &stale_snapshot);
    assert!(!ready_stale);
}

#[test]
fn checkpoint_context_lines_surface_saved_focus() {
    let snapshot = suite_packet_core::AgentSnapshotPayload {
        latest_checkpoint_id: Some("cp-42".to_string()),
        checkpoint_note: Some("Seeded shuffle plan".to_string()),
        checkpoint_focus_paths: vec![
            "apache/src/main/java/org/apache/commons/lang3/ArrayUtils.java".to_string(),
        ],
        checkpoint_focus_symbols: vec!["shuffle".to_string()],
        ..suite_packet_core::AgentSnapshotPayload::default()
    };

    let rendered = render_checkpoint_context_lines(&snapshot);

    assert!(rendered
        .iter()
        .any(|line| line.contains("checkpoint: cp-42")));
    assert!(rendered
        .iter()
        .any(|line| line.contains("note: Seeded shuffle plan")));
    assert!(rendered.iter().any(|line| line
        .contains("focus path: apache/src/main/java/org/apache/commons/lang3/ArrayUtils.java")));
    assert!(rendered
        .iter()
        .any(|line| line.contains("focus symbol: shuffle")));
}
