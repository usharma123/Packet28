use serde::Serialize;
use serde_json::{json, Value};

pub const PACKET_TYPE_COVER_CHECK: &str = "suite.cover.check.v1";
pub const PACKET_TYPE_DIFF_ANALYZE: &str = "suite.diff.analyze.v1";
pub const PACKET_TYPE_TEST_IMPACT: &str = "suite.test.impact.v1";
pub const PACKET_TYPE_AGENT_STATE: &str = "suite.agent.state.v1";
pub const PACKET_TYPE_AGENT_SNAPSHOT: &str = "suite.agent.snapshot.v1";
pub const PACKET_TYPE_CONTEXT_CORRELATE: &str = "suite.context.correlate.v1";
pub const PACKET_TYPE_STACK_SLICE: &str = "suite.stack.slice.v1";
pub const PACKET_TYPE_BUILD_REDUCE: &str = "suite.build.reduce.v1";
pub const PACKET_TYPE_MAP_REPO: &str = "suite.map.repo.v1";
pub const PACKET_TYPE_PROXY_RUN: &str = "suite.proxy.run.v1";
pub const PACKET_TYPE_CONTEXT_ASSEMBLE: &str = "suite.context.assemble.v1";
pub const PACKET_TYPE_GUARD_CHECK: &str = "suite.guard.check.v1";

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PacketTypeContract {
    pub packet_type: &'static str,
    pub required_payload_fields: &'static [&'static str],
    pub optional_payload_fields: &'static [&'static str],
    pub boundedness_rules: &'static [&'static str],
    pub compatibility_notes: &'static [&'static str],
}

impl Default for PacketTypeContract {
    fn default() -> Self {
        Self {
            packet_type: "",
            required_payload_fields: &[],
            optional_payload_fields: &[],
            boundedness_rules: &[],
            compatibility_notes: &[],
        }
    }
}

static CONTRACTS: &[PacketTypeContract] = &[
    PacketTypeContract {
        packet_type: PACKET_TYPE_COVER_CHECK,
        required_payload_fields: &["passed", "violations"],
        optional_payload_fields: &["issue_counts", "debug", "truncated", "artifact_handle"],
        boundedness_rules: &["gate summary only", "bounded issue counts"],
        compatibility_notes: &["legacy envelope_v1/gate_result shim supported for one release"],
    },
    PacketTypeContract {
        packet_type: PACKET_TYPE_DIFF_ANALYZE,
        required_payload_fields: &["gate_result"],
        optional_payload_fields: &[
            "diagnostics",
            "diffs",
            "debug",
            "truncated",
            "returned_count",
            "total_count",
            "artifact_handle",
        ],
        boundedness_rules: &[
            "diff arrays truncated in compact profile",
            "handle profile for full diffs",
        ],
        compatibility_notes: &["legacy gate-json shape supported for one release"],
    },
    PacketTypeContract {
        packet_type: PACKET_TYPE_TEST_IMPACT,
        required_payload_fields: &["result", "known_tests"],
        optional_payload_fields: &[
            "print_command",
            "debug",
            "truncated",
            "returned_count",
            "total_count",
            "artifact_handle",
        ],
        boundedness_rules: &["test arrays truncated in compact profile"],
        compatibility_notes: &["legacy impact_result top-level fields supported for one release"],
    },
    PacketTypeContract {
        packet_type: PACKET_TYPE_AGENT_STATE,
        required_payload_fields: &[
            "task_id",
            "event_id",
            "occurred_at_unix",
            "actor",
            "kind",
            "data",
        ],
        optional_payload_fields: &["paths", "symbols", "debug", "artifact_handle"],
        boundedness_rules: &["single event payload", "bounded path and symbol refs"],
        compatibility_notes: &["v1 task-state event contract"],
    },
    PacketTypeContract {
        packet_type: PACKET_TYPE_AGENT_SNAPSHOT,
        required_payload_fields: &[
            "task_id",
            "focus_paths",
            "focus_symbols",
            "files_read",
            "files_edited",
            "active_decisions",
            "completed_steps",
            "open_questions",
            "event_count",
        ],
        optional_payload_fields: &["last_event_at_unix", "debug", "artifact_handle"],
        boundedness_rules: &[
            "snapshot lists are task scoped",
            "compact mode keeps bounded payload",
        ],
        compatibility_notes: &["v1 derived task-state snapshot contract"],
    },
    PacketTypeContract {
        packet_type: PACKET_TYPE_CONTEXT_CORRELATE,
        required_payload_fields: &["finding_count", "findings"],
        optional_payload_fields: &["task_id", "debug", "artifact_handle"],
        boundedness_rules: &["finding list bounded by deterministic rule set"],
        compatibility_notes: &["v1 deterministic cross-packet correlation contract"],
    },
    PacketTypeContract {
        packet_type: PACKET_TYPE_STACK_SLICE,
        required_payload_fields: &["unique_failures", "total_failures"],
        optional_payload_fields: &[
            "failures",
            "debug",
            "truncated",
            "returned_count",
            "total_count",
            "artifact_handle",
        ],
        boundedness_rules: &["failure list truncated in compact profile"],
        compatibility_notes: &["legacy stack packet shape supported for one release"],
    },
    PacketTypeContract {
        packet_type: PACKET_TYPE_BUILD_REDUCE,
        required_payload_fields: &["unique_diagnostics", "total_diagnostics"],
        optional_payload_fields: &[
            "groups",
            "ordered_fixes",
            "debug",
            "truncated",
            "returned_count",
            "total_count",
            "artifact_handle",
        ],
        boundedness_rules: &["diagnostic groups truncated in compact profile"],
        compatibility_notes: &["legacy build packet shape supported for one release"],
    },
    PacketTypeContract {
        packet_type: PACKET_TYPE_MAP_REPO,
        required_payload_fields: &["files_ranked", "symbols_ranked", "truncation"],
        optional_payload_fields: &["edges", "focus_hits", "debug", "artifact_handle"],
        boundedness_rules: &[
            "rank arrays bounded by request limits",
            "compact mode keeps bounded payload",
        ],
        compatibility_notes: &["legacy packet-detail rich path supported for one release"],
    },
    PacketTypeContract {
        packet_type: PACKET_TYPE_PROXY_RUN,
        required_payload_fields: &["command", "exit_code", "highlights"],
        optional_payload_fields: &[
            "output_lines",
            "groups",
            "dropped",
            "debug",
            "artifact_handle",
        ],
        boundedness_rules: &["line and byte caps enforced before packet emission"],
        compatibility_notes: &["legacy packet-detail rich path supported for one release"],
    },
    PacketTypeContract {
        packet_type: PACKET_TYPE_CONTEXT_ASSEMBLE,
        required_payload_fields: &["sections", "refs", "truncated"],
        optional_payload_fields: &["sources", "debug", "artifact_handle"],
        boundedness_rules: &["budget bounded by tokens/bytes"],
        compatibility_notes: &["legacy final_packet envelope shim supported for one release"],
    },
    PacketTypeContract {
        packet_type: PACKET_TYPE_GUARD_CHECK,
        required_payload_fields: &["passed", "findings"],
        optional_payload_fields: &["debug", "artifact_handle"],
        boundedness_rules: &["findings array compacted in compact profile"],
        compatibility_notes: &["legacy audit-only output supported for one release"],
    },
];

pub fn packet_contracts() -> &'static [PacketTypeContract] {
    CONTRACTS
}

pub fn packet_contract(packet_type: &str) -> Option<&'static PacketTypeContract> {
    CONTRACTS
        .iter()
        .find(|contract| contract.packet_type == packet_type)
}

pub fn wrapper_schema_snapshot() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "suite.packet.v1.schema.json",
        "title": "Suite Packet Wrapper V1",
        "type": "object",
        "required": ["schema_version", "packet_type", "packet"],
        "properties": {
            "schema_version": { "const": "suite.packet.v1" },
            "packet_type": { "type": "string", "pattern": "^suite\\.[a-z0-9]+\\.[a-z0-9]+\\.v1$" },
            "packet": {
                "type": "object",
                "required": ["version", "tool", "kind", "hash", "summary", "budget_cost", "provenance", "payload"],
                "properties": {
                    "version": { "type": "string" },
                    "tool": { "type": "string" },
                    "kind": { "type": "string" },
                    "hash": { "type": "string" },
                    "summary": { "type": "string" },
                    "budget_cost": { "type": "object" },
                    "provenance": { "type": "object" },
                    "payload": {}
                }
            }
        }
    })
}

pub fn packet_type_schema_snapshot(packet_type: &str) -> Option<Value> {
    let contract = packet_contract(packet_type)?;
    Some(json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": format!("{packet_type}.schema.json"),
        "title": format!("Packet Type Contract: {packet_type}"),
        "type": "object",
        "required": ["schema_version", "packet_type", "packet"],
        "properties": {
            "schema_version": { "const": "suite.packet.v1" },
            "packet_type": { "const": packet_type },
            "packet": {
                "type": "object",
                "required": ["version", "tool", "kind", "hash", "summary", "budget_cost", "provenance", "payload"],
                "properties": {
                    "payload": {
                        "type": "object",
                        "required": contract.required_payload_fields,
                        "properties": payload_property_hints(contract),
                        "additionalProperties": true
                    }
                },
                "additionalProperties": true
            }
        },
        "x-boundedness_rules": contract.boundedness_rules,
        "x-compatibility_notes": contract.compatibility_notes,
    }))
}

fn payload_property_hints(contract: &PacketTypeContract) -> Value {
    let mut properties = serde_json::Map::new();
    for field in contract.required_payload_fields {
        properties.insert((*field).to_string(), json!({}));
    }
    for field in contract.optional_payload_fields {
        properties.insert((*field).to_string(), json!({}));
    }
    Value::Object(properties)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn registry_contains_all_phase_one_packet_types() {
        let expected = [
            PACKET_TYPE_COVER_CHECK,
            PACKET_TYPE_DIFF_ANALYZE,
            PACKET_TYPE_TEST_IMPACT,
            PACKET_TYPE_AGENT_STATE,
            PACKET_TYPE_AGENT_SNAPSHOT,
            PACKET_TYPE_CONTEXT_CORRELATE,
            PACKET_TYPE_STACK_SLICE,
            PACKET_TYPE_BUILD_REDUCE,
            PACKET_TYPE_MAP_REPO,
            PACKET_TYPE_PROXY_RUN,
            PACKET_TYPE_CONTEXT_ASSEMBLE,
            PACKET_TYPE_GUARD_CHECK,
        ];
        for packet_type in expected {
            assert!(
                packet_contract(packet_type).is_some(),
                "missing packet contract for {packet_type}"
            );
        }
        assert_eq!(packet_contracts().len(), expected.len());
    }

    #[test]
    fn wrapper_schema_snapshot_is_v1_contract() {
        let schema = wrapper_schema_snapshot();
        assert_eq!(
            schema
                .get("properties")
                .and_then(|v| v.get("schema_version"))
                .and_then(|v| v.get("const"))
                .and_then(Value::as_str),
            Some("suite.packet.v1")
        );
        assert!(schema
            .get("required")
            .and_then(Value::as_array)
            .is_some_and(|fields| fields.iter().any(|f| f.as_str() == Some("packet_type"))));
    }

    #[test]
    fn packet_type_schema_snapshot_contains_payload_requirements() {
        let schema = packet_type_schema_snapshot(PACKET_TYPE_DIFF_ANALYZE).expect("schema exists");
        assert_eq!(
            schema
                .get("properties")
                .and_then(|v| v.get("packet_type"))
                .and_then(|v| v.get("const"))
                .and_then(Value::as_str),
            Some(PACKET_TYPE_DIFF_ANALYZE)
        );
        let required = schema
            .get("properties")
            .and_then(|v| v.get("packet"))
            .and_then(|v| v.get("properties"))
            .and_then(|v| v.get("payload"))
            .and_then(|v| v.get("required"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        assert!(required
            .iter()
            .any(|field| field.as_str() == Some("gate_result")));
    }

    #[test]
    fn schema_artifacts_exist_for_all_packet_types_and_profiles() {
        let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();

        let wrapper_path =
            workspace_root.join("schemas/packet-wrapper/suite.packet.v1.schema.json");
        let wrapper_value: Value =
            serde_json::from_str(&std::fs::read_to_string(wrapper_path).unwrap()).unwrap();
        assert_eq!(
            wrapper_value
                .get("properties")
                .and_then(|v| v.get("schema_version"))
                .and_then(|v| v.get("const"))
                .and_then(Value::as_str),
            Some("suite.packet.v1")
        );

        let expected_packet_types = [
            PACKET_TYPE_COVER_CHECK,
            PACKET_TYPE_DIFF_ANALYZE,
            PACKET_TYPE_TEST_IMPACT,
            PACKET_TYPE_AGENT_STATE,
            PACKET_TYPE_AGENT_SNAPSHOT,
            PACKET_TYPE_CONTEXT_CORRELATE,
            PACKET_TYPE_STACK_SLICE,
            PACKET_TYPE_BUILD_REDUCE,
            PACKET_TYPE_MAP_REPO,
            PACKET_TYPE_PROXY_RUN,
            PACKET_TYPE_CONTEXT_ASSEMBLE,
            PACKET_TYPE_GUARD_CHECK,
        ];

        for packet_type in expected_packet_types {
            let schema_path =
                workspace_root.join(format!("schemas/packet-types/{packet_type}.schema.json"));
            let schema: Value =
                serde_json::from_str(&std::fs::read_to_string(schema_path).unwrap()).unwrap();
            assert_eq!(
                schema
                    .get("properties")
                    .and_then(|v| v.get("packet_type"))
                    .and_then(|v| v.get("const"))
                    .and_then(Value::as_str),
                Some(packet_type)
            );

            for profile in ["compact", "full", "handle"] {
                let snapshot_path =
                    workspace_root.join(format!("schemas/snapshots/{packet_type}/{profile}.json"));
                let snapshot: Value =
                    serde_json::from_str(&std::fs::read_to_string(snapshot_path).unwrap()).unwrap();
                assert_eq!(
                    snapshot.get("schema_version").and_then(Value::as_str),
                    Some("suite.packet.v1")
                );
                assert_eq!(
                    snapshot.get("packet_type").and_then(Value::as_str),
                    Some(packet_type)
                );
            }
        }
    }
}
