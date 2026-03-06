use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct CorrelationEvidenceRef {
    pub packet_id: Option<String>,
    pub packet_type: String,
    pub kind: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct ContextCorrelationFinding {
    pub rule: String,
    pub relation: String,
    pub confidence: f64,
    pub summary: String,
    pub evidence_refs: Vec<CorrelationEvidenceRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct ContextCorrelationPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub finding_count: usize,
    pub findings: Vec<ContextCorrelationFinding>,
}
