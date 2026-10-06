//! Stable, implementation-free wire contract for Packet28 daemon clients.
//!
//! Runtime, persistence, kernel, memory, and search implementations deliberately
//! live outside this crate. Clients can depend on these request/response types,
//! framing helpers, and deterministic endpoint paths without pulling in the
//! daemon implementation graph.
//!
//! # Compatibility and migration
//!
//! [`DaemonRequest`] and [`DaemonResponse`] are the only crate-root wire
//! re-exports retained for `0.2.x` source compatibility. New code should use
//! the named modules. In particular, bounded status and registry traversal use
//! [`registry`] rather than extending the frozen legacy message enums.
//! Existing `packet28-daemon-core` root imports remain available through that
//! crate's frozen `0.2.x` facade; connection code should migrate to
//! `packet28-daemon-client`.
//!
//! # Errors
//!
//! [`frame::read_frame`] and [`frame::write_frame`] return
//! [`frame::FrameError`] without erasing their I/O or JSON sources. Runtime
//! request failures remain wire-compatible [`DaemonResponse::Error`] strings;
//! clients must not infer a stable typed error kind from those strings.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use suite_packet_core::kernel::{
    KernelRequest, KernelResponse, KernelSequenceRequest, KernelSequenceResponse,
};
use suite_packet_core::memory::{
    ContextStoreEntryDetail, ContextStoreEntrySummary, ContextStorePruneReport, ContextStoreStats,
    RecallHit,
};

pub mod broker;
pub mod commands;
pub mod context_store;
pub mod frame;
pub mod hooks;
pub mod index;
pub mod logging;
pub mod message;
pub mod paths;
pub mod process;
pub mod registry;
pub mod task;

use broker::*;
use commands::*;
use context_store::*;
use hooks::*;
use index::*;
use task::*;

pub use message::{DaemonRequest, DaemonResponse};
