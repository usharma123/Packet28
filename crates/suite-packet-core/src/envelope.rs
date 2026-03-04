use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct FileRef {
    pub path: String,
    pub relevance: Option<f64>,
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct SymbolRef {
    pub name: String,
    pub file: Option<String>,
    pub relevance: Option<f64>,
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct BudgetCost {
    pub est_tokens: u64,
    pub est_bytes: usize,
    pub runtime_ms: u64,
    pub tool_calls: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct Provenance {
    pub inputs: Vec<String>,
    pub git_base: Option<String>,
    pub git_head: Option<String>,
    pub generated_at_unix: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct EnvelopeV1<T> {
    pub version: String,
    pub tool: String,
    pub kind: String,
    pub hash: String,
    pub summary: String,
    pub files: Vec<FileRef>,
    pub symbols: Vec<SymbolRef>,
    pub risk: Option<RiskLevel>,
    pub confidence: Option<f64>,
    pub budget_cost: BudgetCost,
    pub provenance: Provenance,
    pub payload: T,
}

impl<T: Default> Default for EnvelopeV1<T> {
    fn default() -> Self {
        Self {
            version: "1".to_string(),
            tool: String::new(),
            kind: String::new(),
            hash: String::new(),
            summary: String::new(),
            files: Vec::new(),
            symbols: Vec::new(),
            risk: None,
            confidence: None,
            budget_cost: BudgetCost::default(),
            provenance: Provenance::default(),
            payload: T::default(),
        }
    }
}

impl<T: Serialize> EnvelopeV1<T> {
    pub fn canonical_hash(&self) -> String {
        let mut value = serde_json::to_value(self).unwrap_or(Value::Null);
        if let Some(obj) = value.as_object_mut() {
            obj.insert("hash".to_string(), Value::String(String::new()));
            if let Some(budget_cost) = obj.get_mut("budget_cost").and_then(Value::as_object_mut) {
                budget_cost.insert("runtime_ms".to_string(), Value::from(0));
            }
            if let Some(provenance) = obj.get_mut("provenance").and_then(Value::as_object_mut) {
                provenance.insert("generated_at_unix".to_string(), Value::from(0));
            }
        }
        canonical_hash_json(&value)
    }

    pub fn with_canonical_hash(mut self) -> Self {
        self.files
            .sort_by(|a, b| a.path.cmp(&b.path).then_with(|| cmp_opt_f64(a.relevance, b.relevance)));
        self.symbols.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then_with(|| a.file.cmp(&b.file))
                .then_with(|| cmp_opt_f64(a.relevance, b.relevance))
        });
        self.hash = self.canonical_hash();
        self
    }
}

pub fn canonical_hash_json<T: Serialize>(value: &T) -> String {
    let as_value = serde_json::to_value(value).unwrap_or(Value::Null);
    let canonical = canonicalize_value(as_value);
    let bytes = serde_json::to_vec(&canonical).unwrap_or_default();
    blake3::hash(&bytes).to_hex().to_string()
}

fn canonicalize_value(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut ordered = BTreeMap::new();
            for (k, v) in map {
                ordered.insert(k, canonicalize_value(v));
            }
            let mut out = serde_json::Map::new();
            for (k, v) in ordered {
                out.insert(k, v);
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(canonicalize_value).collect()),
        other => other,
    }
}

fn cmp_opt_f64(a: Option<f64>, b: Option<f64>) -> std::cmp::Ordering {
    match (a, b) {
        (Some(a), Some(b)) => a.total_cmp(&b),
        (Some(_), None) => std::cmp::Ordering::Greater,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_hash_is_stable_for_key_order() {
        let left = json!({
            "b": 2,
            "a": {
                "y": 2,
                "x": 1
            }
        });
        let right = json!({
            "a": {
                "x": 1,
                "y": 2
            },
            "b": 2
        });

        assert_eq!(canonical_hash_json(&left), canonical_hash_json(&right));
    }

    #[test]
    fn envelope_sets_hash_after_sorting_refs() {
        let env = EnvelopeV1 {
            version: "1".to_string(),
            tool: "demo".to_string(),
            kind: "demo.kind".to_string(),
            hash: String::new(),
            summary: "demo".to_string(),
            files: vec![
                FileRef {
                    path: "b".to_string(),
                    relevance: Some(0.1),
                    source: None,
                },
                FileRef {
                    path: "a".to_string(),
                    relevance: Some(0.9),
                    source: None,
                },
            ],
            symbols: vec![
                SymbolRef {
                    name: "b".to_string(),
                    file: None,
                    relevance: None,
                    source: None,
                },
                SymbolRef {
                    name: "a".to_string(),
                    file: None,
                    relevance: None,
                    source: None,
                },
            ],
            risk: None,
            confidence: Some(1.0),
            budget_cost: BudgetCost::default(),
            provenance: Provenance::default(),
            payload: json!({"ok": true}),
        }
        .with_canonical_hash();

        assert!(!env.hash.is_empty());
        assert_eq!(env.files[0].path, "a");
        assert_eq!(env.symbols[0].name, "a");
    }
}
