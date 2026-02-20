use std::collections::BTreeSet;

use crate::model::FileDiff;
use crate::testmap::TestMapIndex;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct ImpactResult {
    pub selected_tests: Vec<String>,
    pub smoke_tests: Vec<String>,
    pub missing_mappings: Vec<String>,
    pub stale: bool,
    pub confidence: f64,
    pub escalate_full_suite: bool,
}

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

#[cfg(test)]
mod tests {
    use roaring::RoaringBitmap;

    use super::*;
    use crate::model::{DiffStatus, FileDiff};

    fn diff(path: &str) -> FileDiff {
        FileDiff {
            path: path.to_string(),
            old_path: None,
            status: DiffStatus::Modified,
            changed_lines: RoaringBitmap::new(),
        }
    }

    #[test]
    fn test_select_impacted_tests_from_inverse_index() {
        let mut map = TestMapIndex::default();
        map.file_to_tests
            .entry("src/a.rs".to_string())
            .or_default()
            .insert("tests::a".to_string());

        let result = select_impacted_tests(&map, &[diff("src/a.rs")]);
        assert_eq!(result.selected_tests, vec!["tests::a".to_string()]);
        assert!(result.missing_mappings.is_empty());
    }

    #[test]
    fn test_select_impacted_tests_missing_mapping() {
        let map = TestMapIndex::default();
        let result = select_impacted_tests(&map, &[diff("src/missing.rs")]);
        assert!(result.selected_tests.is_empty());
        assert_eq!(result.missing_mappings, vec!["src/missing.rs".to_string()]);
    }
}
