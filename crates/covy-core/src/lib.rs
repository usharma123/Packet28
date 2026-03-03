//! Temporary compatibility shim.
//!
//! `covy-cli` now depends on domain crates directly
//! (`diffy-core`, `testy-core`, `suite-foundation-core`, `suite-packet-core`).
//! Keep this crate as a backward-compatible re-export surface only.
//! Do not add new orchestration or business logic here.

pub mod model {
    pub use suite_packet_core::coverage::*;
    pub use suite_packet_core::gate::*;
}
pub use model::*;

pub mod config {
    pub use suite_foundation_core::config::*;
}
pub use config::CovyConfig;

pub mod error {
    pub use suite_packet_core::error::*;
}
pub use error::CovyError;

pub mod diagnostics {
    pub use suite_packet_core::diagnostics::*;
}

pub mod pathmap {
    pub use suite_foundation_core::pathmap::*;
}

pub mod path_diagnose {
    pub use suite_foundation_core::path_diagnose::*;
}

pub mod snapshot {
    pub use suite_foundation_core::snapshot::*;
}

pub mod cache {
    pub use suite_foundation_core::cache::*;
}

pub mod diff {
    pub use diffy_core::diff::*;
}

pub mod gate {
    pub use diffy_core::gate::*;
}

pub mod report {
    pub use diffy_core::report::*;
}

pub mod pipeline {
    pub use diffy_core::pipeline::*;
}

pub mod impact {
    pub use testy_core::impact::*;
}

pub mod impact_pipeline {
    pub use testy_core::pipeline::*;
}

pub mod testmap {
    pub use suite_packet_core::testmap::*;
}

pub mod testmap_pipeline {
    pub use testy_core::pipeline_testmap::*;
}

pub mod merge {
    pub use testy_core::merge::*;
}

pub mod shard {
    pub use testy_core::shard::*;
}

pub mod shard_pipeline {
    pub use testy_core::pipeline_shard::*;
}

pub mod shard_timing {
    pub use testy_core::shard_timing::*;
}
