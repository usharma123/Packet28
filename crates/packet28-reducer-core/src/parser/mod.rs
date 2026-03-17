//! Three-tier parser infrastructure for command output reduction.
//!
//! Every reducer should target `ParseResult::Full` for structured extraction,
//! fall back to `ParseResult::Degraded` when partial structure is available,
//! and return `ParseResult::Passthrough` only when no structure can be extracted.

pub mod types;

/// Three-tier parse result with graceful degradation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseResult<T> {
    /// Fully structured output was extracted.
    Full(T),
    /// Partial structure extracted; warnings describe what was missed.
    Degraded(T, Vec<String>),
    /// No structure could be extracted; raw output passed through.
    Passthrough(String),
}

impl<T> ParseResult<T> {
    /// Returns the inner value regardless of tier, consuming the result.
    pub fn into_value(self) -> Option<T> {
        match self {
            ParseResult::Full(v) | ParseResult::Degraded(v, _) => Some(v),
            ParseResult::Passthrough(_) => None,
        }
    }

    /// True when the result carries a structured value (Full or Degraded).
    pub fn is_structured(&self) -> bool {
        !matches!(self, ParseResult::Passthrough(_))
    }

    /// Map the inner value, preserving the tier.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> ParseResult<U> {
        match self {
            ParseResult::Full(v) => ParseResult::Full(f(v)),
            ParseResult::Degraded(v, w) => ParseResult::Degraded(f(v), w),
            ParseResult::Passthrough(s) => ParseResult::Passthrough(s),
        }
    }
}

/// Trait for parsers that extract structured output from command text.
pub trait OutputParser {
    type Output;
    fn parse(input: &str) -> ParseResult<Self::Output>;
}

/// Output format modes for reduced output rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FormatMode {
    #[default]
    Compact,
    Detailed,
    Json,
}

/// Extract the first top-level JSON object from input (brace-balanced).
pub fn extract_json_object(input: &str) -> Option<&str> {
    let start = input.find('{')?;
    let bytes = input.as_bytes();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes[start..].iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        match b {
            b'\\' if in_string => escaped = true,
            b'"' => in_string = !in_string,
            b'{' if !in_string => depth += 1,
            b'}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(&input[start..start + i + 1]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Extract the first top-level JSON array from input (bracket-balanced).
pub fn extract_json_array(input: &str) -> Option<&str> {
    let start = input.find('[')?;
    let bytes = input.as_bytes();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes[start..].iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        match b {
            b'\\' if in_string => escaped = true,
            b'"' => in_string = !in_string,
            b'[' if !in_string => depth += 1,
            b']' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(&input[start..start + i + 1]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Truncate a string to `max` characters, appending "..." if truncated.
pub fn truncate_output(s: &str, max: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max {
        s.to_string()
    } else if max <= 3 {
        "...".to_string()
    } else {
        let shortened: String = s.chars().take(max - 3).collect();
        format!("{shortened}...")
    }
}

/// Strip ANSI escape codes from text.
pub fn strip_ansi(text: &str) -> String {
    // Match ESC[ ... m (SGR) and ESC[ ... other CSI sequences
    let mut result = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'[') {
            // Skip until we find a letter (the terminator)
            i += 2;
            while i < bytes.len() && !(bytes[i].is_ascii_alphabetic() || bytes[i] == b'@') {
                i += 1;
            }
            if i < bytes.len() {
                i += 1; // skip the terminator
            }
        } else {
            result.push(bytes[i] as char);
            i += 1;
        }
    }
    result
}

/// Collapse consecutive blank lines into at most one.
pub fn collapse_blank_lines(text: &str) -> String {
    let mut result = Vec::new();
    let mut prev_blank = false;
    for line in text.lines() {
        let is_blank = line.trim().is_empty();
        if is_blank && prev_blank {
            continue;
        }
        result.push(line);
        prev_blank = is_blank;
    }
    result.join("\n")
}

/// Collapse repeated consecutive lines, replacing runs with `[xN] line`.
pub fn collapse_repeated_lines(lines: &[&str]) -> Vec<String> {
    let mut result = Vec::new();
    let mut prev: Option<&str> = None;
    let mut count = 0usize;
    for &line in lines {
        if prev == Some(line) {
            count += 1;
        } else {
            if let Some(prev_line) = prev {
                if count > 1 {
                    result.push(format!("[x{}] {}", count, prev_line));
                } else {
                    result.push(prev_line.to_string());
                }
            }
            prev = Some(line);
            count = 1;
        }
    }
    if let Some(prev_line) = prev {
        if count > 1 {
            result.push(format!("[x{}] {}", count, prev_line));
        } else {
            result.push(prev_line.to_string());
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_json_object_basic() {
        let input = r#"some prefix {"key": "value", "nested": {"a": 1}} trailing"#;
        assert_eq!(
            extract_json_object(input),
            Some(r#"{"key": "value", "nested": {"a": 1}}"#)
        );
    }

    #[test]
    fn extract_json_object_with_strings() {
        let input = r#"{"msg": "hello {world}"}"#;
        assert_eq!(extract_json_object(input), Some(input));
    }

    #[test]
    fn extract_json_object_none_when_missing() {
        assert_eq!(extract_json_object("no json here"), None);
    }

    #[test]
    fn extract_json_array_basic() {
        let input = r#"prefix [1, 2, [3, 4]] suffix"#;
        assert_eq!(extract_json_array(input), Some("[1, 2, [3, 4]]"));
    }

    #[test]
    fn truncate_output_short() {
        assert_eq!(truncate_output("hello", 10), "hello");
    }

    #[test]
    fn truncate_output_exact() {
        assert_eq!(truncate_output("hello", 5), "hello");
    }

    #[test]
    fn truncate_output_long() {
        assert_eq!(truncate_output("hello world", 8), "hello...");
    }

    #[test]
    fn strip_ansi_removes_color_codes() {
        let input = "\x1b[31merror\x1b[0m: something failed";
        assert_eq!(strip_ansi(input), "error: something failed");
    }

    #[test]
    fn strip_ansi_preserves_plain_text() {
        assert_eq!(strip_ansi("plain text"), "plain text");
    }

    #[test]
    fn collapse_blank_lines_works() {
        let input = "a\n\n\n\nb\n\nc";
        assert_eq!(collapse_blank_lines(input), "a\n\nb\n\nc");
    }

    #[test]
    fn collapse_repeated_lines_works() {
        let lines = vec!["a", "b", "b", "b", "c", "c"];
        let result = collapse_repeated_lines(&lines);
        assert_eq!(result, vec!["a", "[x3] b", "[x2] c"]);
    }

    #[test]
    fn parse_result_into_value() {
        let full: ParseResult<i32> = ParseResult::Full(42);
        assert_eq!(full.into_value(), Some(42));

        let passthrough: ParseResult<i32> = ParseResult::Passthrough("raw".to_string());
        assert_eq!(passthrough.into_value(), None);
    }

    #[test]
    fn parse_result_map() {
        let full: ParseResult<i32> = ParseResult::Full(42);
        let mapped = full.map(|v| v * 2);
        assert_eq!(mapped.into_value(), Some(84));
    }
}
