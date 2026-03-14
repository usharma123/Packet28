use super::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffAnalyzeKernelInput {
    pub base: String,
    pub head: String,
    pub fail_under_changed: Option<f64>,
    pub fail_under_total: Option<f64>,
    pub fail_under_new: Option<f64>,
    pub max_new_errors: Option<u32>,
    pub max_new_warnings: Option<u32>,
    pub max_new_issues: Option<u32>,
    pub issues: Vec<String>,
    pub issues_state: Option<String>,
    pub no_issues_state: bool,
    pub coverage: Vec<String>,
    pub input: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffAnalyzeKernelOutput {
    pub gate_result: suite_packet_core::QualityGateResult,
    pub diagnostics: Option<suite_packet_core::DiagnosticsData>,
    pub diffs: Vec<SerializableFileDiff>,
}

impl Default for DiffAnalyzeKernelOutput {
    fn default() -> Self {
        Self {
            gate_result: suite_packet_core::QualityGateResult {
                passed: false,
                total_coverage_pct: None,
                changed_coverage_pct: None,
                new_file_coverage_pct: None,
                violations: Vec::new(),
                issue_counts: None,
            },
            diagnostics: None,
            diffs: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializableFileDiff {
    pub path: String,
    pub old_path: Option<String>,
    pub status: suite_packet_core::DiffStatus,
    pub changed_lines: Vec<u32>,
}

impl SerializableFileDiff {
    pub fn from_file_diff(diff: &suite_packet_core::FileDiff) -> Self {
        Self {
            path: diff.path.clone(),
            old_path: diff.old_path.clone(),
            status: diff.status,
            changed_lines: diff.changed_lines.iter().collect(),
        }
    }

    pub fn into_file_diff(self) -> suite_packet_core::FileDiff {
        let mut bitmap = RoaringBitmap::new();
        for line in self.changed_lines {
            bitmap.insert(line);
        }

        suite_packet_core::FileDiff {
            path: self.path,
            old_path: self.old_path,
            status: self.status,
            changed_lines: bitmap,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpactKernelInput {
    pub base: Option<String>,
    pub head: Option<String>,
    pub testmap: String,
    pub print_command: bool,
    pub config_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ImpactKernelOutput {
    pub result: suite_packet_core::ImpactResult,
    pub known_tests: usize,
    pub print_command: Option<String>,
}

fn format_pct(value: Option<f64>) -> String {
    value
        .map(|pct| format!("{pct:.2}"))
        .unwrap_or_else(|| "n/a".to_string())
}

pub(crate) fn default_diff_pipeline_ingest_adapters() -> diffy_core::pipeline::PipelineIngestAdapters {
    diffy_core::pipeline::PipelineIngestAdapters {
        ingest_coverage_auto,
        ingest_coverage_with_format,
        ingest_coverage_stdin,
        ingest_diagnostics,
    }
}

fn ingest_coverage_auto(path: &Path) -> anyhow::Result<diffy_core::model::CoverageData> {
    covy_ingest::ingest_path(path).map_err(Into::into)
}

fn ingest_coverage_with_format(
    path: &Path,
    format: diffy_core::model::CoverageFormat,
) -> anyhow::Result<diffy_core::model::CoverageData> {
    covy_ingest::ingest_path_with_format(path, format).map_err(Into::into)
}

fn ingest_coverage_stdin(
    format: diffy_core::model::CoverageFormat,
) -> anyhow::Result<diffy_core::model::CoverageData> {
    covy_ingest::ingest_reader(std::io::stdin().lock(), format).map_err(Into::into)
}

fn ingest_diagnostics(path: &Path) -> anyhow::Result<diffy_core::diagnostics::DiagnosticsData> {
    covy_ingest::ingest_diagnostics_path(path).map_err(Into::into)
}

pub fn build_diff_pipeline_request(
    input: &DiffAnalyzeKernelInput,
) -> diffy_core::pipeline::PipelineRequest {
    diffy_core::pipeline::PipelineRequest {
        base: input.base.clone(),
        head: input.head.clone(),
        source_root: None,
        coverage: diffy_core::pipeline::PipelineCoverageInput {
            paths: input.coverage.clone(),
            format: None,
            stdin: false,
            input_state_path: input.input.clone(),
            default_input_state_path: Some(".covy/state/latest.bin".to_string()),
            strip_prefixes: Vec::new(),
            reject_paths_with_input: false,
            no_inputs_error: "No coverage data found. Run `covy ingest` first or use --coverage."
                .to_string(),
        },
        diagnostics: diffy_core::pipeline::PipelineDiagnosticsInput {
            issue_patterns: input.issues.clone(),
            issues_state_path: input.issues_state.clone(),
            no_issues_state: input.no_issues_state,
            default_issues_state_path: ".covy/state/issues.bin".to_string(),
        },
        gate: suite_foundation_core::config::GateConfig {
            fail_under_total: input.fail_under_total,
            fail_under_changed: input.fail_under_changed,
            fail_under_new: input.fail_under_new,
            issues: suite_foundation_core::config::IssueGateConfig {
                max_new_errors: input.max_new_errors,
                max_new_warnings: input.max_new_warnings,
                max_new_issues: input.max_new_issues,
            },
        },
    }
}

pub fn build_diff_analyze_envelope(
    output: &diffy_core::pipeline::PipelineOutput,
    base: &str,
    head: &str,
) -> suite_packet_core::EnvelopeV1<DiffAnalyzeKernelOutput> {
    let kernel_output = DiffAnalyzeKernelOutput {
        gate_result: output.gate_result.clone(),
        diagnostics: output.diagnostics.clone(),
        diffs: output
            .changed_line_context
            .diffs
            .iter()
            .map(SerializableFileDiff::from_file_diff)
            .collect(),
    };

    let mut changed_paths = output
        .changed_line_context
        .changed_paths
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    changed_paths.sort();

    let gate_summary = format!(
        "passed: {}\nchanged_coverage_pct: {}\ntotal_coverage_pct: {}\nnew_file_coverage_pct: {}\nviolations: {}",
        kernel_output.gate_result.passed,
        format_pct(kernel_output.gate_result.changed_coverage_pct),
        format_pct(kernel_output.gate_result.total_coverage_pct),
        format_pct(kernel_output.gate_result.new_file_coverage_pct),
        if kernel_output.gate_result.violations.is_empty() {
            "none".to_string()
        } else {
            kernel_output.gate_result.violations.join("; ")
        }
    );

    let changed_file_body = if changed_paths.is_empty() {
        "No changed files".to_string()
    } else {
        changed_paths.join("\n")
    };

    let files = changed_paths
        .iter()
        .map(|path| suite_packet_core::FileRef {
            path: path.clone(),
            relevance: Some(0.75),
            source: Some("diffy.analyze".to_string()),
        })
        .collect::<Vec<_>>();
    let payload_bytes = serde_json::to_vec(&kernel_output).unwrap_or_default().len();

    suite_packet_core::EnvelopeV1 {
        version: "1".to_string(),
        tool: "diffy".to_string(),
        kind: "diff_analyze".to_string(),
        hash: String::new(),
        summary: format!("{gate_summary}\nchanged_files: {changed_file_body}"),
        files,
        symbols: Vec::new(),
        risk: None,
        confidence: Some(if output.gate_result.passed { 1.0 } else { 0.8 }),
        budget_cost: suite_packet_core::BudgetCost {
            est_tokens: 0,
            est_bytes: 0,
            runtime_ms: 0,
            tool_calls: 1,
            payload_est_tokens: Some((payload_bytes / 4) as u64),
            payload_est_bytes: Some(payload_bytes),
        },
        provenance: suite_packet_core::Provenance {
            inputs: changed_paths,
            git_base: Some(base.to_string()),
            git_head: Some(head.to_string()),
            generated_at_unix: now_unix(),
        },
        payload: kernel_output,
    }
    .with_canonical_hash_and_real_budget()
}

pub fn build_test_impact_envelope(
    output: &testy_core::command_impact::ImpactLegacyOutput,
    testmap_path: &str,
    git_base: Option<&str>,
    git_head: Option<&str>,
) -> suite_packet_core::EnvelopeV1<ImpactKernelOutput> {
    let impact_output = ImpactKernelOutput {
        result: output.result.clone(),
        known_tests: output.known_tests,
        print_command: output.print_command.clone(),
    };

    let mut paths = output.result.missing_mappings.clone();
    paths.sort();
    paths.dedup();

    let mut symbol_refs = output.result.selected_tests.clone();
    symbol_refs.extend(output.result.smoke_tests.clone());
    symbol_refs.sort();
    symbol_refs.dedup();

    let summary = format!(
        "selected: {}\nknown: {}\nmissing: {}\nconfidence: {:.2}\nstale: {}\nescalate_full_suite: {}",
        output.result.selected_tests.len(),
        output.known_tests,
        output.result.missing_mappings.len(),
        output.result.confidence,
        output.result.stale,
        output.result.escalate_full_suite,
    );

    let files = paths
        .iter()
        .map(|path: &String| suite_packet_core::FileRef {
            path: path.clone(),
            relevance: Some(0.8),
            source: Some("testy.impact".to_string()),
        })
        .collect::<Vec<_>>();
    let symbols = symbol_refs
        .iter()
        .map(|symbol: &String| suite_packet_core::SymbolRef {
            name: symbol.clone(),
            file: None,
            kind: Some("test_id".to_string()),
            relevance: Some(0.8),
            source: Some("testy.impact".to_string()),
        })
        .collect::<Vec<_>>();

    let payload_bytes = serde_json::to_vec(&impact_output).unwrap_or_default().len();

    suite_packet_core::EnvelopeV1 {
        version: "1".to_string(),
        tool: "testy".to_string(),
        kind: "test_impact".to_string(),
        hash: String::new(),
        summary,
        files,
        symbols,
        risk: None,
        confidence: Some(output.result.confidence.clamp(0.0, 1.0)),
        budget_cost: suite_packet_core::BudgetCost {
            est_tokens: 0,
            est_bytes: 0,
            runtime_ms: 0,
            tool_calls: 1,
            payload_est_tokens: Some((payload_bytes / 4) as u64),
            payload_est_bytes: Some(payload_bytes),
        },
        provenance: suite_packet_core::Provenance {
            inputs: vec![testmap_path.to_string()],
            git_base: git_base.map(ToOwned::to_owned),
            git_head: git_head.map(ToOwned::to_owned),
            generated_at_unix: now_unix(),
        },
        payload: impact_output,
    }
    .with_canonical_hash_and_real_budget()
}
