use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use roaring::RoaringBitmap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use thiserror::Error;

use context_memory_core::{
    basename_alias, normalize_context_path, CachePacket, DeltaReuseHooks, NoopDeltaReuseHooks,
    PacketCache, RecallHit, RecallOptions, RecallScope,
};

pub use context_memory_core::PersistConfig;

mod agenty_runtime;
mod broker_memory_runtime;
mod contextq_runtime;
mod correlation_runtime;
mod diff_runtime;
mod governance_runtime;
mod kernel_registry;
mod kernel_runtime;
mod kernel_types;
mod reactive_runtime;
mod tool_reducers_runtime;

pub(crate) use agenty_runtime::*;
pub(crate) use broker_memory_runtime::*;
pub(crate) use contextq_runtime::*;
pub(crate) use correlation_runtime::*;
pub use diff_runtime::*;
pub(crate) use governance_runtime::*;
pub use kernel_registry::*;
pub use kernel_runtime::*;
pub use kernel_types::*;
pub(crate) use reactive_runtime::*;
pub(crate) use tool_reducers_runtime::*;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct ContextAssembleEnvelopePayload {
    sources: Vec<String>,
    sections: Vec<contextq_core::ContextSection>,
    refs: Vec<contextq_core::ContextRef>,
    truncated: bool,
    assembly: contextq_core::AssemblySummary,
    tool_invocations: Vec<contextq_core::ToolInvocation>,
    reducer_invocations: Vec<contextq_core::ReducerInvocation>,
    text_blobs: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    debug: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct ContextManageRequest {
    task_id: String,
    query: Option<String>,
    budget_tokens: u64,
    budget_bytes: usize,
    scope: RecallScope,
    checkpoint_id: Option<String>,
    focus_paths: Vec<String>,
    focus_symbols: Vec<String>,
}

impl Default for ContextManageRequest {
    fn default() -> Self {
        Self {
            task_id: String::new(),
            query: None,
            budget_tokens: contextq_core::DEFAULT_BUDGET_TOKENS,
            budget_bytes: contextq_core::DEFAULT_BUDGET_BYTES,
            scope: RecallScope::TaskFirst,
            checkpoint_id: None,
            focus_paths: Vec::new(),
            focus_symbols: Vec::new(),
        }
    }
}

fn path_matches_any(patterns: &[String], candidate: &str) -> bool {
    patterns.iter().any(|pattern| {
        let pattern = pattern.trim();
        !pattern.is_empty()
            && (candidate == pattern
                || candidate.starts_with(pattern)
                || pattern.starts_with(candidate)
                || candidate.contains(pattern))
    })
}

fn estimate_tokens(bytes: usize) -> u64 {
    (bytes as u64).div_ceil(4)
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn merge_json(left: Value, right: Value) -> Value {
    match (left, right) {
        (Value::Object(mut left), Value::Object(right)) => {
            for (key, value) in right {
                left.insert(key, value);
            }
            Value::Object(left)
        }
        (value, Value::Null) => value,
        (_, value) => value,
    }
}

#[cfg(test)]
mod tests;
