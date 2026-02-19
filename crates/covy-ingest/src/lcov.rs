use covy_core::model::{CoverageData, CoverageFormat, FileCoverage};
use covy_core::CovyError;

use crate::Ingestor;

pub struct LcovIngestor;

impl Ingestor for LcovIngestor {
    fn format(&self) -> CoverageFormat {
        CoverageFormat::Lcov
    }

    fn parse(&self, data: &[u8]) -> Result<CoverageData, CovyError> {
        let text = std::str::from_utf8(data)
            .map_err(|e| CovyError::Parse {
                format: "lcov".into(),
                detail: format!("Invalid UTF-8: {e}"),
            })?;
        parse_lcov(text)
    }
}

fn parse_lcov(text: &str) -> Result<CoverageData, CovyError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(CovyError::EmptyInput {
            path: "(lcov input)".into(),
        });
    }
    // Validate: must contain at least one SF: line
    if !trimmed.contains("SF:") {
        // Check if it looks like XML (wrong format)
        if trimmed.starts_with('<') {
            return Err(CovyError::Parse {
                format: "lcov".into(),
                detail: "Input looks like XML — did you mean --format cobertura or --format jacoco?".into(),
            });
        }
        return Err(CovyError::Parse {
            format: "lcov".into(),
            detail: "No SF: (source file) lines found".into(),
        });
    }

    let mut result = CoverageData::new();
    result.format = Some(CoverageFormat::Lcov);

    let mut current_file: Option<String> = None;
    let mut current_coverage = FileCoverage::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        if let Some(path) = line.strip_prefix("SF:") {
            // Start new file
            current_file = Some(path.to_string());
            current_coverage = FileCoverage::new();
        } else if line.starts_with("DA:") {
            // DA:line_number,execution_count[,checksum]
            let parts: Vec<&str> = line[3..].splitn(3, ',').collect();
            if parts.len() >= 2 {
                if let Ok(line_no) = parts[0].parse::<u32>() {
                    let count: u64 = parts[1].parse().unwrap_or(0);
                    current_coverage.lines_instrumented.insert(line_no);
                    if count > 0 {
                        current_coverage.lines_covered.insert(line_no);
                    }
                }
            }
        } else if line.starts_with("BRDA:") {
            // BRDA:line,block,branch,taken
            let parts: Vec<&str> = line[5..].splitn(4, ',').collect();
            if parts.len() >= 4 {
                if let (Ok(line_no), Ok(block)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                    let taken: u64 = parts[3].parse().unwrap_or(0);
                    current_coverage.branches.insert((line_no, block), taken);
                }
            }
        } else if line.starts_with("FN:") {
            // FN:line_number,function_name — just record it
        } else if line.starts_with("FNDA:") {
            // FNDA:execution_count,function_name
            let parts: Vec<&str> = line[5..].splitn(2, ',').collect();
            if parts.len() == 2 {
                let count: u64 = parts[0].parse().unwrap_or(0);
                current_coverage
                    .functions
                    .insert(parts[1].to_string(), count);
            }
        } else if line == "end_of_record" {
            if let Some(path) = current_file.take() {
                result
                    .files
                    .entry(path)
                    .or_insert_with(FileCoverage::new)
                    .merge(&current_coverage);
                current_coverage = FileCoverage::new();
            }
        }
        // Ignore TN:, LH:, LF:, BRF:, BRH:, FNF:, FNH: (summary lines)
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_basic_lcov() {
        let lcov = r#"TN:test
SF:src/main.rs
FN:1,main
FNDA:1,main
DA:1,1
DA:2,1
DA:3,0
DA:4,1
BRDA:2,0,0,1
BRDA:2,0,1,0
LF:4
LH:3
end_of_record
SF:src/lib.rs
DA:1,0
DA:2,0
LF:2
LH:0
end_of_record
"#;

        let result = parse_lcov(lcov).unwrap();
        assert_eq!(result.files.len(), 2);

        let main = &result.files["src/main.rs"];
        assert_eq!(main.lines_instrumented.len(), 4);
        assert_eq!(main.lines_covered.len(), 3);
        assert!(main.lines_covered.contains(1));
        assert!(!main.lines_covered.contains(3));

        let lib = &result.files["src/lib.rs"];
        assert_eq!(lib.lines_instrumented.len(), 2);
        assert_eq!(lib.lines_covered.len(), 0);
    }

    #[test]
    fn test_parse_empty() {
        let result = parse_lcov("");
        assert!(result.is_err());
    }

    #[test]
    fn test_merge_same_file() {
        let lcov = r#"SF:a.rs
DA:1,1
DA:2,0
end_of_record
SF:a.rs
DA:2,1
DA:3,1
end_of_record
"#;
        let result = parse_lcov(lcov).unwrap();
        assert_eq!(result.files.len(), 1);
        let fc = &result.files["a.rs"];
        assert_eq!(fc.lines_covered.len(), 3); // 1, 2, 3 all covered via merge
        assert_eq!(fc.lines_instrumented.len(), 3);
    }
}
