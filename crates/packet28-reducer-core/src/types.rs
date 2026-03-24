use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct SearchRequest {
    pub query: String,
    pub requested_paths: Vec<String>,
    pub fixed_string: bool,
    pub case_sensitive: Option<bool>,
    pub whole_word: bool,
    pub context_lines: Option<usize>,
    pub max_matches_per_file: Option<usize>,
    pub max_total_matches: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct SearchMatch {
    pub path: String,
    pub line: usize,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct SearchGroup {
    pub path: String,
    pub match_count: usize,
    pub displayed_match_count: usize,
    pub truncated: bool,
    pub matches: Vec<SearchMatch>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct SearchResult {
    pub query: String,
    pub requested_paths: Vec<String>,
    pub resolved_paths: Vec<String>,
    pub match_count: usize,
    pub returned_match_count: usize,
    pub truncated: bool,
    pub paths: Vec<String>,
    pub regions: Vec<String>,
    pub symbols: Vec<String>,
    pub groups: Vec<SearchGroup>,
    pub compact_preview: String,
    pub diagnostics: Vec<String>,
    pub engine: Option<SearchEngineStats>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct SearchEngineStats {
    pub engine: String,
    pub index_generation: Option<u64>,
    pub base_commit: Option<String>,
    pub plan_kind: Option<String>,
    pub planner_fallback: Option<String>,
    pub stale_reason: Option<String>,
    pub candidates_examined: usize,
    pub candidate_files: usize,
    pub verified_files: usize,
    pub index_lookups: usize,
    pub postings_bytes_read: u64,
    pub fallback_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct ReadRegionsRequest {
    pub path: String,
    pub regions: Vec<String>,
    pub line_start: Option<usize>,
    pub line_end: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct ReadLine {
    pub line: usize,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct ReadRegionsResult {
    pub path: String,
    pub regions: Vec<String>,
    pub symbols: Vec<String>,
    pub lines: Vec<ReadLine>,
    pub compact_preview: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommandReducerFamily {
    Git,
    Fs,
    Rust,
    Github,
    Python,
    Javascript,
    Go,
    Infra,
}

impl CommandReducerFamily {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Fs => "fs",
            Self::Rust => "rust",
            Self::Github => "github",
            Self::Python => "python",
            Self::Javascript => "javascript",
            Self::Go => "go",
            Self::Infra => "infra",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct CommandReducerSpec {
    pub family: String,
    pub canonical_kind: String,
    pub packet_type: String,
    pub operation_kind: suite_packet_core::ToolOperationKind,
    pub command: String,
    pub argv: Vec<String>,
    pub cache_fingerprint: String,
    pub cacheable: bool,
    pub mutation: bool,
    pub paths: Vec<String>,
    pub equivalence_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct CommandReduction {
    pub family: String,
    pub canonical_kind: String,
    pub packet_type: String,
    pub operation_kind: suite_packet_core::ToolOperationKind,
    pub summary: String,
    /// Condensed readable preview (e.g. RTK-style compact diff) for agent context.
    pub compact_preview: String,
    pub paths: Vec<String>,
    pub regions: Vec<String>,
    pub symbols: Vec<String>,
    pub failed: bool,
    pub error_class: Option<String>,
    pub error_message: Option<String>,
    pub retryable: Option<bool>,
    pub exit_code: i32,
    pub cache_fingerprint: String,
    pub cacheable: bool,
    pub mutation: bool,
    pub equivalence_key: Option<String>,
}
