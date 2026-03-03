#[path = "coverage_store.rs"]
mod coverage_store;
#[path = "diagnostics_store.rs"]
mod diagnostics_store;
#[path = "gate_cache.rs"]
mod gate_cache;
#[path = "repo_identity.rs"]
mod repo_identity;
#[path = "testmap_store.rs"]
mod testmap_store;
#[path = "timing_store.rs"]
mod timing_store;

pub use coverage_store::{deserialize_coverage, serialize_coverage};
pub use diagnostics_store::{
    deserialize_diagnostics, deserialize_diagnostics_for_paths,
    deserialize_diagnostics_for_paths_from_file, deserialize_diagnostics_with_metadata,
    serialize_diagnostics, serialize_diagnostics_with_metadata, DiagnosticsStateMetadata,
    DIAGNOSTICS_PATH_NORM_VERSION, DIAGNOSTICS_STATE_SCHEMA_VERSION,
};
pub use gate_cache::{CachedResult, CoverageCache};
pub use repo_identity::current_repo_root_id;
pub use testmap_store::{deserialize_testmap, serialize_testmap, TESTMAP_SCHEMA_VERSION};
pub use timing_store::{
    deserialize_test_timings, serialize_test_timings, TESTTIMINGS_SCHEMA_VERSION,
};
