use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct BuildReduceRequest {
    pub log_text: String,
    pub source: Option<String>,
    pub max_diagnostics: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct BuildDiagnostic {
    pub file: String,
    pub line: u32,
    pub column: u32,
    pub severity: String,
    pub code: Option<String>,
    pub message: String,
    pub fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RootCauseGroup {
    pub root_cause: String,
    pub severity: String,
    pub count: usize,
    pub diagnostics: Vec<BuildDiagnostic>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BuildReduceOutput {
    pub schema_version: String,
    pub source: Option<String>,
    pub total_diagnostics: usize,
    pub unique_diagnostics: usize,
    pub duplicates_removed: usize,
    pub groups: Vec<RootCauseGroup>,
    pub ordered_fixes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BuildPacket {
    pub packet_id: Option<String>,
    pub tool: Option<String>,
    pub tools: Vec<String>,
    pub reducer: Option<String>,
    pub reducers: Vec<String>,
    pub paths: Vec<String>,
    pub payload: serde_json::Value,
    pub sections: Vec<serde_json::Value>,
    pub refs: Vec<serde_json::Value>,
    pub text_blobs: Vec<String>,
}
