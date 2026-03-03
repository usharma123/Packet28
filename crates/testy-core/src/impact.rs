use std::collections::{BTreeMap, BTreeSet, HashSet};

use roaring::RoaringBitmap;

use crate::model::FileDiff;
use crate::testmap::TestMapIndex;

pub use suite_packet_core::gate::{ImpactPlan, ImpactResult, PlannedTest, UncoveredBlock};

pub fn select_impacted_tests(index: &TestMapIndex, diffs: &[FileDiff]) -> ImpactResult {
    let mut tests: BTreeSet<String> = BTreeSet::new();
    let mut missing = BTreeSet::new();

    for diff in diffs {
        let mut matched = false;
        if let Some(candidates) = index.file_to_tests.get(&diff.path) {
            for t in candidates {
                tests.insert(t.clone());
            }
            matched = true;
        }

        if let Some(old_path) = diff.old_path.as_deref() {
            if let Some(candidates) = index.file_to_tests.get(old_path) {
                for t in candidates {
                    tests.insert(t.clone());
                }
                matched = true;
            }
        }

        if !matched {
            missing.insert(diff.path.clone());
        }
    }

    ImpactResult {
        selected_tests: tests.into_iter().collect(),
        smoke_tests: Vec::new(),
        missing_mappings: missing.into_iter().collect(),
        stale: false,
        confidence: 1.0,
        escalate_full_suite: false,
    }
}

pub fn plan_impacted_tests(
    index: &TestMapIndex,
    diffs: &[FileDiff],
    max_tests: usize,
    target_coverage: f64,
) -> ImpactPlan {
    let mut plan = ImpactPlan {
        next_command: "covy impact run --plan plan.json -- <your-test-command-template>"
            .to_string(),
        ..Default::default()
    };

    let file_to_idx: BTreeMap<&str, usize> = index
        .file_index
        .iter()
        .enumerate()
        .map(|(i, f)| (f.as_str(), i))
        .collect();

    let mut mapped_remaining: BTreeMap<usize, RoaringBitmap> = BTreeMap::new();
    let mut mapped_file_names: BTreeMap<usize, String> = BTreeMap::new();
    let mut unmapped_remaining: BTreeMap<String, RoaringBitmap> = BTreeMap::new();

    for diff in diffs {
        let changed = diff.changed_lines.clone();
        if changed.is_empty() {
            continue;
        }

        let mapped_idx = file_to_idx.get(diff.path.as_str()).copied().or_else(|| {
            diff.old_path
                .as_deref()
                .and_then(|p| file_to_idx.get(p).copied())
        });

        if let Some(file_idx) = mapped_idx {
            mapped_file_names
                .entry(file_idx)
                .or_insert_with(|| index.file_index[file_idx].clone());
            mapped_remaining
                .entry(file_idx)
                .or_insert_with(RoaringBitmap::new)
                .extend(changed.iter());
        } else {
            unmapped_remaining
                .entry(diff.path.clone())
                .or_insert_with(RoaringBitmap::new)
                .extend(changed.iter());
        }
    }

    plan.changed_lines_total =
        total_bitmap_lines(&mapped_remaining) + total_bitmap_lines_by_name(&unmapped_remaining);
    if plan.changed_lines_total == 0 {
        plan.plan_coverage_pct = 1.0;
        return plan;
    }

    let original_mapped = mapped_remaining.clone();

    let mut test_rows: Vec<Vec<RoaringBitmap>> = Vec::new();
    for row in &index.coverage {
        let mut bitmap_row = Vec::with_capacity(row.len());
        for lines in row {
            let mut bm = RoaringBitmap::new();
            for line in lines {
                bm.insert(*line);
            }
            bitmap_row.push(bm);
        }
        test_rows.push(bitmap_row);
    }

    let max_index_tests = index.tests.len().min(test_rows.len());
    let mut selected: HashSet<usize> = HashSet::new();

    while plan.tests.len() < max_tests && selected.len() < max_index_tests {
        let mut best: Option<(usize, u64, u64, String)> = None; // idx, gain, overlap, id

        for test_idx in 0..max_index_tests {
            if selected.contains(&test_idx) {
                continue;
            }
            let gain = test_gain_against_remaining(test_rows.get(test_idx), &mapped_remaining);
            if gain == 0 {
                continue;
            }
            let overlap = test_gain_against_remaining(test_rows.get(test_idx), &original_mapped);
            let id = index.tests[test_idx].clone();

            best = match best {
                None => Some((test_idx, gain, overlap, id)),
                Some((best_idx, best_gain, best_overlap, best_id)) => {
                    if gain > best_gain
                        || (gain == best_gain && overlap > best_overlap)
                        || (gain == best_gain && overlap == best_overlap && id < best_id)
                    {
                        Some((test_idx, gain, overlap, id))
                    } else {
                        Some((best_idx, best_gain, best_overlap, best_id))
                    }
                }
            };
        }

        let Some((winner_idx, winner_gain, winner_overlap, winner_id)) = best else {
            break;
        };

        selected.insert(winner_idx);
        subtract_test_from_remaining(test_rows.get(winner_idx), &mut mapped_remaining);

        plan.tests.push(PlannedTest {
            id: winner_id.clone(),
            name: winner_id,
            estimated_overlap_lines: winner_overlap,
            marginal_gain_lines: winner_gain,
        });

        let remaining_total =
            total_bitmap_lines(&mapped_remaining) + total_bitmap_lines_by_name(&unmapped_remaining);
        plan.changed_lines_covered_by_plan =
            plan.changed_lines_total.saturating_sub(remaining_total);
        plan.plan_coverage_pct =
            plan.changed_lines_covered_by_plan as f64 / plan.changed_lines_total as f64;

        if plan.plan_coverage_pct >= target_coverage {
            break;
        }
    }

    if plan.changed_lines_total > 0 {
        let remaining_total =
            total_bitmap_lines(&mapped_remaining) + total_bitmap_lines_by_name(&unmapped_remaining);
        plan.changed_lines_covered_by_plan =
            plan.changed_lines_total.saturating_sub(remaining_total);
        plan.plan_coverage_pct =
            plan.changed_lines_covered_by_plan as f64 / plan.changed_lines_total as f64;
    }

    plan.uncovered_blocks =
        build_uncovered_blocks(&mapped_remaining, &mapped_file_names, &unmapped_remaining);
    plan
}

fn total_bitmap_lines(map: &BTreeMap<usize, RoaringBitmap>) -> u64 {
    map.values().map(|b| b.len()).sum()
}

fn total_bitmap_lines_by_name(map: &BTreeMap<String, RoaringBitmap>) -> u64 {
    map.values().map(|b| b.len()).sum()
}

fn test_gain_against_remaining(
    test_row: Option<&Vec<RoaringBitmap>>,
    mapped_remaining: &BTreeMap<usize, RoaringBitmap>,
) -> u64 {
    let Some(test_row) = test_row else {
        return 0;
    };
    let mut gain = 0u64;
    for (file_idx, remaining) in mapped_remaining {
        if let Some(test_lines) = test_row.get(*file_idx) {
            gain += (&remaining.clone() & test_lines).len();
        }
    }
    gain
}

fn subtract_test_from_remaining(
    test_row: Option<&Vec<RoaringBitmap>>,
    mapped_remaining: &mut BTreeMap<usize, RoaringBitmap>,
) {
    let Some(test_row) = test_row else {
        return;
    };

    let keys: Vec<usize> = mapped_remaining.keys().copied().collect();
    for file_idx in keys {
        if let Some(test_lines) = test_row.get(file_idx) {
            if let Some(remaining) = mapped_remaining.get_mut(&file_idx) {
                *remaining -= test_lines;
                if remaining.is_empty() {
                    mapped_remaining.remove(&file_idx);
                }
            }
        }
    }
}

fn build_uncovered_blocks(
    mapped_remaining: &BTreeMap<usize, RoaringBitmap>,
    mapped_file_names: &BTreeMap<usize, String>,
    unmapped_remaining: &BTreeMap<String, RoaringBitmap>,
) -> Vec<UncoveredBlock> {
    let mut by_file: BTreeMap<String, RoaringBitmap> = BTreeMap::new();

    for (file_idx, lines) in mapped_remaining {
        if let Some(name) = mapped_file_names.get(file_idx) {
            by_file.insert(name.clone(), lines.clone());
        }
    }
    for (file, lines) in unmapped_remaining {
        by_file.insert(file.clone(), lines.clone());
    }

    let mut blocks = Vec::new();
    for (file, lines) in by_file {
        let vec_lines: Vec<u32> = lines.iter().collect();
        if vec_lines.is_empty() {
            continue;
        }

        let mut start = vec_lines[0];
        let mut end = vec_lines[0];
        for line in vec_lines.iter().skip(1) {
            if *line == end + 1 {
                end = *line;
            } else {
                blocks.push(UncoveredBlock {
                    file: file.clone(),
                    start_line: start,
                    end_line: end,
                });
                start = *line;
                end = *line;
            }
        }
        blocks.push(UncoveredBlock {
            file,
            start_line: start,
            end_line: end,
        });
    }

    blocks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DiffStatus, FileDiff};

    fn diff_with_lines(path: &str, lines: &[u32]) -> FileDiff {
        let mut changed_lines = RoaringBitmap::new();
        for line in lines {
            changed_lines.insert(*line);
        }
        FileDiff {
            path: path.to_string(),
            old_path: None,
            status: DiffStatus::Modified,
            changed_lines,
        }
    }

    fn basic_v2_map() -> TestMapIndex {
        let mut map = TestMapIndex::default();
        map.tests = vec!["t1".to_string(), "t2".to_string(), "t3".to_string()];
        map.file_index = vec!["src/a.rs".to_string(), "src/b.rs".to_string()];
        map.coverage = vec![
            vec![vec![10, 11], vec![]],
            vec![vec![11], vec![20, 21]],
            vec![vec![10], vec![20]],
        ];
        map
    }

    #[test]
    fn test_select_impacted_tests_from_inverse_index() {
        let mut map = TestMapIndex::default();
        map.file_to_tests
            .entry("src/a.rs".to_string())
            .or_default()
            .insert("tests::a".to_string());

        let result = select_impacted_tests(&map, &[diff_with_lines("src/a.rs", &[])]);
        assert_eq!(result.selected_tests, vec!["tests::a".to_string()]);
        assert!(result.missing_mappings.is_empty());
    }

    #[test]
    fn test_select_impacted_tests_missing_mapping() {
        let map = TestMapIndex::default();
        let result = select_impacted_tests(&map, &[diff_with_lines("src/missing.rs", &[])]);
        assert!(result.selected_tests.is_empty());
        assert_eq!(result.missing_mappings, vec!["src/missing.rs".to_string()]);
    }

    #[test]
    fn test_plan_impacted_tests_greedy_and_coverage() {
        let map = basic_v2_map();
        let diffs = vec![
            diff_with_lines("src/a.rs", &[10, 11, 12]),
            diff_with_lines("src/b.rs", &[20, 21]),
        ];

        let plan = plan_impacted_tests(&map, &diffs, 2, 0.8);
        assert_eq!(plan.changed_lines_total, 5);
        assert_eq!(plan.tests.len(), 2);
        assert_eq!(plan.tests[0].id, "t2");
        assert!(plan.plan_coverage_pct >= 0.8);
    }

    #[test]
    fn test_plan_impacted_tests_deterministic_tie_break() {
        let mut map = TestMapIndex::default();
        map.tests = vec!["aaa".to_string(), "bbb".to_string()];
        map.file_index = vec!["src/a.rs".to_string()];
        map.coverage = vec![vec![vec![1]], vec![vec![1]]];
        let diffs = vec![diff_with_lines("src/a.rs", &[1])];

        let plan = plan_impacted_tests(&map, &diffs, 1, 1.0);
        assert_eq!(plan.tests[0].id, "aaa");
    }

    #[test]
    fn test_uncovered_blocks_compact_ranges() {
        let mut map = TestMapIndex::default();
        map.tests = vec!["t1".to_string()];
        map.file_index = vec!["src/a.rs".to_string()];
        map.coverage = vec![vec![vec![1, 2]]];
        let diffs = vec![diff_with_lines("src/a.rs", &[1, 2, 3, 5])];

        let plan = plan_impacted_tests(&map, &diffs, 1, 1.0);
        assert_eq!(plan.uncovered_blocks.len(), 2);
        assert_eq!(plan.uncovered_blocks[0].start_line, 3);
        assert_eq!(plan.uncovered_blocks[0].end_line, 3);
        assert_eq!(plan.uncovered_blocks[1].start_line, 5);
    }
}
