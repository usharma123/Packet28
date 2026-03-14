pub mod error {
    pub use suite_packet_core::error::*;
}

use serde::{Deserialize, Serialize};
use serde_json::Value;

mod assemble;

pub use assemble::{assemble_packet_files, assemble_packets};

pub const DEFAULT_BUDGET_TOKENS: u64 = 5_000;
pub const DEFAULT_BUDGET_BYTES: usize = 32_000;
pub const CONTEXTQ_SCHEMA_VERSION: &str = "contextq.assemble.v1";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DetailMode {
    #[default]
    Compact,
    Rich,
}

#[derive(Debug, Clone)]
pub struct AssembleOptions {
    pub budget_tokens: u64,
    pub budget_bytes: usize,
    pub detail_mode: DetailMode,
    pub compact_assembly: bool,
    pub agent_snapshot: Option<suite_packet_core::AgentSnapshotPayload>,
}

impl Default for AssembleOptions {
    fn default() -> Self {
        Self {
            budget_tokens: DEFAULT_BUDGET_TOKENS,
            budget_bytes: DEFAULT_BUDGET_BYTES,
            detail_mode: DetailMode::Compact,
            compact_assembly: false,
            agent_snapshot: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ContextRef {
    pub kind: String,
    pub value: String,
    pub source: Option<String>,
    pub relevance: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ContextSection {
    pub id: Option<String>,
    pub title: String,
    pub body: String,
    pub refs: Vec<ContextRef>,
    pub relevance: Option<f64>,
    pub source_packet: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ToolInvocation {
    pub name: String,
    pub reducer: Option<String>,
    pub paths: Vec<String>,
    pub token_usage: Option<u64>,
    pub runtime_ms: Option<u64>,
    pub input: Value,
    pub output: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ReducerInvocation {
    pub name: String,
    pub token_usage: Option<u64>,
    pub runtime_ms: Option<u64>,
    pub output: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PacketFileRef {
    pub path: String,
    pub relevance: Option<f64>,
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PacketSymbolRef {
    pub name: String,
    pub file: Option<String>,
    pub kind: Option<String>,
    pub relevance: Option<f64>,
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct InputPacket {
    pub packet_id: Option<String>,
    pub summary: Option<String>,
    pub tool: Option<String>,
    pub tools: Vec<String>,
    pub reducer: Option<String>,
    pub reducers: Vec<String>,
    pub paths: Vec<String>,
    pub token_usage: Option<u64>,
    pub runtime_ms: Option<u64>,
    pub payload: Value,
    pub files: Vec<PacketFileRef>,
    pub symbols: Vec<PacketSymbolRef>,
    pub tool_invocations: Vec<ToolInvocation>,
    pub reducer_invocations: Vec<ReducerInvocation>,
    pub text_blobs: Vec<String>,
    pub sections: Vec<ContextSection>,
    pub refs: Vec<ContextRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AssemblySummary {
    pub input_packets: usize,
    pub sections_input: usize,
    pub sections_kept: usize,
    pub sections_dropped: usize,
    pub refs_input: usize,
    pub refs_kept: usize,
    pub refs_dropped: usize,
    pub budget_tokens: u64,
    pub budget_bytes: usize,
    pub estimated_tokens: u64,
    pub estimated_bytes: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AssembledPayload {
    pub sources: Vec<String>,
    pub sections: Vec<ContextSection>,
    pub refs: Vec<ContextRef>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AssembledPacket {
    pub schema_version: String,
    pub packet_id: Option<String>,
    pub tool: Option<String>,
    pub tools: Vec<String>,
    pub reducer: Option<String>,
    pub reducers: Vec<String>,
    pub paths: Vec<String>,
    pub token_usage: Option<u64>,
    pub runtime_ms: Option<u64>,
    pub payload: Value,
    pub tool_invocations: Vec<ToolInvocation>,
    pub reducer_invocations: Vec<ReducerInvocation>,
    pub text_blobs: Vec<String>,
    pub assembly: AssemblySummary,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    #[test]
    fn assemble_dedupes_refs_and_keeps_higher_ranked_sections() {
        let packet_a = InputPacket {
            packet_id: Some("diffy".to_string()),
            sections: vec![ContextSection {
                title: "Critical failure in src/lib.rs".to_string(),
                body: "error on uncovered lines".to_string(),
                refs: vec![ContextRef {
                    kind: "file".to_string(),
                    value: "src/lib.rs".to_string(),
                    source: Some("diffy".to_string()),
                    relevance: Some(0.9),
                }],
                relevance: Some(1.2),
                ..ContextSection::default()
            }],
            ..InputPacket::default()
        };

        let packet_b = InputPacket {
            packet_id: Some("impact".to_string()),
            sections: vec![ContextSection {
                title: "Impacted tests".to_string(),
                body: "selected tests list".to_string(),
                refs: vec![ContextRef {
                    kind: "file".to_string(),
                    value: "src/lib.rs".to_string(),
                    source: Some("impact".to_string()),
                    relevance: Some(0.7),
                }],
                relevance: Some(0.6),
                ..ContextSection::default()
            }],
            ..InputPacket::default()
        };

        let assembled = assemble_packets(
            vec![packet_a, packet_b],
            AssembleOptions {
                budget_tokens: 1000,
                budget_bytes: 50_000,
                ..AssembleOptions::default()
            },
        );

        assert_eq!(assembled.schema_version, CONTEXTQ_SCHEMA_VERSION);
        assert_eq!(assembled.assembly.input_packets, 2);
        let payload: AssembledPayload = serde_json::from_value(assembled.payload).unwrap();
        assert_eq!(payload.refs.len(), 1);
        assert_eq!(payload.sections.len(), 2);
    }

    #[test]
    fn assemble_respects_tight_budget() {
        let long = "x".repeat(4000);
        let packet = InputPacket {
            packet_id: Some("diffy".to_string()),
            sections: vec![ContextSection {
                title: "Very large section".to_string(),
                body: long,
                relevance: Some(1.0),
                ..ContextSection::default()
            }],
            ..InputPacket::default()
        };

        let assembled = assemble_packets(
            vec![packet],
            AssembleOptions {
                budget_tokens: 60,
                budget_bytes: 500,
                ..AssembleOptions::default()
            },
        );

        assert_eq!(assembled.schema_version, CONTEXTQ_SCHEMA_VERSION);
        assert!(assembled.assembly.truncated);
        assert!(assembled.token_usage.unwrap_or(0) <= 60);
        assert!(assembled.assembly.estimated_bytes <= 500);
    }

    #[test]
    fn assemble_packet_files_reads_inputs() {
        let dir = TempDir::new().unwrap();
        let a = dir.path().join("a.json");
        let b = dir.path().join("b.json");

        std::fs::write(
            &a,
            r#"{"packet_id":"a","paths":["src/a.rs"],"payload":{"selected_tests":["foo::bar"]}}"#,
        )
        .unwrap();
        std::fs::write(
            &b,
            r#"{"packet_id":"b","payload":{"paths":["src/a.rs","src/b.rs"]}}"#,
        )
        .unwrap();

        let assembled = assemble_packet_files(
            &[a, b],
            AssembleOptions {
                budget_tokens: 1200,
                budget_bytes: 24_000,
                ..AssembleOptions::default()
            },
        )
        .unwrap();

        assert_eq!(assembled.assembly.input_packets, 2);
        assert!(assembled.paths.contains(&"src/a.rs".to_string()));
    }

    #[test]
    fn derives_refs_from_envelope_top_level_refs() {
        let packet = InputPacket {
            packet_id: Some("mapy".to_string()),
            files: vec![PacketFileRef {
                path: "src/main.rs".to_string(),
                relevance: Some(0.9),
                source: Some("mapy.repo".to_string()),
            }],
            symbols: vec![PacketSymbolRef {
                name: "run".to_string(),
                file: Some("src/main.rs".to_string()),
                kind: Some("function".to_string()),
                relevance: Some(0.8),
                source: Some("mapy.repo".to_string()),
            }],
            ..InputPacket::default()
        };

        let assembled = assemble_packets(
            vec![packet],
            AssembleOptions {
                budget_tokens: 1200,
                budget_bytes: 24_000,
                ..AssembleOptions::default()
            },
        );
        let payload: AssembledPayload = serde_json::from_value(assembled.payload).unwrap();

        assert!(payload
            .refs
            .iter()
            .any(|r| r.kind == "file" && r.value == "src/main.rs"));
        assert!(payload
            .refs
            .iter()
            .any(|r| r.kind == "symbol" && r.value == "run"));
    }

    #[test]
    fn default_section_body_uses_pretty_json_in_rich_mode() {
        let packet = InputPacket {
            packet_id: Some("mapy".to_string()),
            payload: json!({
                "alpha": "beta",
                "items": [1, 2],
            }),
            ..InputPacket::default()
        };

        let assembled = assemble_packets(
            vec![packet],
            AssembleOptions {
                detail_mode: DetailMode::Rich,
                ..AssembleOptions::default()
            },
        );
        let payload: AssembledPayload = serde_json::from_value(assembled.payload).unwrap();
        let body = &payload.sections[0].body;
        assert!(body.contains('\n'));
        assert!(body.contains("  \"alpha\""));
    }

    #[test]
    fn default_section_body_prefers_packet_summary_in_compact_mode() {
        let packet = InputPacket {
            packet_id: Some("mapy".to_string()),
            summary: Some("repo_map files=4 symbols=24 edges=0".to_string()),
            payload: json!({
                "files_ranked": [{"file_idx": 0, "score": 0.9}],
                "symbols_ranked": [{"symbol_idx": 0, "score": 0.8}],
                "edges": []
            }),
            ..InputPacket::default()
        };

        let assembled = assemble_packets(
            vec![packet],
            AssembleOptions {
                detail_mode: DetailMode::Compact,
                ..AssembleOptions::default()
            },
        );
        let payload: AssembledPayload = serde_json::from_value(assembled.payload).unwrap();
        assert_eq!(
            payload.sections[0].body,
            "repo_map files=4 symbols=24 edges=0"
        );
    }

    #[test]
    fn compact_assembly_drops_duplicate_section_refs_and_text_blobs() {
        let packet = InputPacket {
            packet_id: Some("diffy".to_string()),
            sections: vec![ContextSection {
                title: "Diff".to_string(),
                body: "critical regression".to_string(),
                refs: vec![ContextRef {
                    kind: "file".to_string(),
                    value: "src/lib.rs".to_string(),
                    source: Some("diffy".to_string()),
                    relevance: Some(0.9),
                }],
                ..ContextSection::default()
            }],
            ..InputPacket::default()
        };

        let assembled = assemble_packets(
            vec![packet],
            AssembleOptions {
                compact_assembly: true,
                ..AssembleOptions::default()
            },
        );
        let payload: AssembledPayload = serde_json::from_value(assembled.payload).unwrap();
        assert_eq!(assembled.text_blobs.len(), 0);
        assert!(assembled.tool_invocations.is_empty());
        assert!(assembled.reducer_invocations.is_empty());
        assert_eq!(payload.sections.len(), 1);
        assert!(payload.sections[0].refs.is_empty());
        assert_eq!(payload.refs.len(), 1);
    }

    #[test]
    fn task_aware_assembly_boosts_focus_and_compresses_read_sections() {
        let already_read = InputPacket {
            packet_id: Some("diffy".to_string()),
            sections: vec![ContextSection {
                title: "Diff".to_string(),
                body: "StopWatch.java changed on lines 10-20".to_string(),
                refs: vec![ContextRef {
                    kind: "file".to_string(),
                    value: "src/time/StopWatch.java".to_string(),
                    source: Some("diffy.analyze".to_string()),
                    relevance: Some(0.9),
                }],
                relevance: Some(0.9),
                ..ContextSection::default()
            }],
            ..InputPacket::default()
        };
        let focused = InputPacket {
            packet_id: Some("mapy".to_string()),
            sections: vec![ContextSection {
                title: "Neighbors".to_string(),
                body: "DateUtils references split() in the time package".to_string(),
                refs: vec![
                    ContextRef {
                        kind: "file".to_string(),
                        value: "src/time/DateUtils.java".to_string(),
                        source: Some("mapy.repo".to_string()),
                        relevance: Some(0.7),
                    },
                    ContextRef {
                        kind: "symbol".to_string(),
                        value: "split".to_string(),
                        source: Some("mapy.repo".to_string()),
                        relevance: Some(0.7),
                    },
                ],
                relevance: Some(0.7),
                ..ContextSection::default()
            }],
            ..InputPacket::default()
        };

        let assembled = assemble_packets(
            vec![already_read, focused],
            AssembleOptions {
                budget_tokens: 1200,
                budget_bytes: 24_000,
                agent_snapshot: Some(suite_packet_core::AgentSnapshotPayload {
                    task_id: "task-a".to_string(),
                    focus_paths: vec!["src/time/DateUtils.java".to_string()],
                    focus_symbols: vec!["split".to_string()],
                    files_read: vec!["src/time/StopWatch.java".to_string()],
                    files_edited: Vec::new(),
                    active_decisions: Vec::new(),
                    completed_steps: vec!["read_diff".to_string()],
                    open_questions: vec![suite_packet_core::AgentQuestion {
                        id: "q1".to_string(),
                        text: "Does DateUtils call split()?".to_string(),
                    }],
                    event_count: 3,
                    last_event_at_unix: Some(3),
                    latest_checkpoint_id: None,
                    latest_checkpoint_at_unix: None,
                    changed_paths_since_checkpoint: Vec::new(),
                    changed_symbols_since_checkpoint: Vec::new(),
                    ..suite_packet_core::AgentSnapshotPayload::default()
                }),
                ..AssembleOptions::default()
            },
        );
        let payload: AssembledPayload = serde_json::from_value(assembled.payload).unwrap();

        assert_eq!(payload.sections[0].title, "Neighbors");
        assert!(payload
            .sections
            .iter()
            .any(|section| section.body.starts_with("Reminder: already reviewed")));
    }
}
