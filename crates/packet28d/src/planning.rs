use super::*;

pub(crate) fn normalize_plan_steps(steps: &[BrokerPlanStep]) -> Vec<BrokerPlanStep> {
    steps
        .iter()
        .enumerate()
        .map(|(idx, step)| {
            let mut normalized = step.clone();
            if normalized.id.trim().is_empty() {
                normalized.id = format!("step-{}", idx + 1);
            } else {
                normalized.id = normalized.id.trim().to_string();
            }
            normalized.action = normalized.action.trim().to_ascii_lowercase();
            normalized.description = normalized
                .description
                .as_ref()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty());
            normalized.paths = merged_unique(&[], &step.paths);
            normalized.symbols = merged_unique(&[], &step.symbols);
            normalized.depends_on = merged_unique(&[], &step.depends_on);
            normalized
        })
        .collect()
}

pub(crate) fn is_edit_like_action(action: &str) -> bool {
    matches!(
        action,
        "edit"
            | "file_edit"
            | "rename"
            | "extract"
            | "split_file"
            | "merge_files"
            | "restructure_module"
    )
}

pub(crate) fn is_read_like_action(action: &str) -> bool {
    matches!(action, "read" | "file_read" | "inspect" | "open")
}

pub(crate) fn is_test_like_step(step: &BrokerPlanStep) -> bool {
    step.action.contains("test")
        || step
            .description
            .as_deref()
            .is_some_and(|text| text.to_ascii_lowercase().contains("test"))
        || step.paths.iter().any(|path| {
            let lower = path.to_ascii_lowercase();
            lower.contains("test") || lower.contains("/spec") || lower.ends_with("_test.rs")
        })
}

pub(crate) fn estimate_plan_step_tokens(step: &BrokerPlanStep) -> u64 {
    let mut estimate = 48_u64;
    estimate = estimate.saturating_add((step.paths.len() as u64) * 40);
    estimate = estimate.saturating_add((step.symbols.len() as u64) * 24);
    estimate = estimate.saturating_add((step.depends_on.len() as u64) * 8);
    if let Some(description) = &step.description {
        estimate = estimate.saturating_add(estimate_text_cost(description).0);
    }
    estimate.max(64)
}

pub(crate) fn find_candidate_test_paths(
    path: &str,
    rich_map: &mapy_core::RepoMapPayloadRich,
    testmap: Option<&suite_packet_core::TestMapIndex>,
) -> Vec<String> {
    let lower = path.to_ascii_lowercase();
    let mut candidates = HashMap::<String, usize>::new();
    for file in &rich_map.files_ranked {
        let file_lower = file.path.to_ascii_lowercase();
        if !(file_lower.contains("test") || file_lower.contains("/spec")) {
            continue;
        }
        let score = if file_lower.contains(lower.as_str()) {
            3
        } else if Path::new(&file.path)
            .file_stem()
            .and_then(|value| value.to_str())
            .is_some_and(|stem| lower.contains(&stem.to_ascii_lowercase()))
        {
            2
        } else {
            1
        };
        candidates.insert(file.path.clone(), score);
    }
    if let Some(testmap) = testmap {
        if let Some(mapped) = testmap.file_to_tests.get(path) {
            for test_id in mapped {
                candidates.entry(test_id.clone()).or_insert(4);
            }
        }
    }
    let mut ranked = candidates.into_iter().collect::<Vec<_>>();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.into_iter().map(|(path, _)| path).take(3).collect()
}

pub(crate) fn coverage_gap_for_path(
    coverage: Option<&suite_packet_core::CoverageData>,
    path: &str,
) -> bool {
    let Some(coverage) = coverage else {
        return false;
    };
    coverage
        .files
        .get(path)
        .and_then(|file| file.line_coverage_pct())
        .map(|pct| pct < 80.0)
        .unwrap_or(true)
}

pub(crate) fn current_deleted_paths(root: &Path) -> HashSet<String> {
    let output = Command::new("git")
        .current_dir(root)
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output();
    let Ok(output) = output else {
        return HashSet::new();
    };
    if !output.status.success() {
        return HashSet::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim_end();
            if trimmed.len() < 4 {
                return None;
            }
            let status = &trimmed[..2];
            if !status.contains('D') {
                return None;
            }
            Some(trimmed[3..].trim().to_string())
        })
        .collect()
}

pub(crate) fn merged_unique(current: &[String], requested: &[String]) -> Vec<String> {
    let mut values = std::collections::BTreeSet::new();
    for value in current {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            values.insert(trimmed.to_string());
        }
    }
    for value in requested {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            values.insert(trimmed.to_string());
        }
    }
    values.into_iter().collect()
}
