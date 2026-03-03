use std::path::Path;

use anyhow::Result;
use suite_packet_core::{CoverageData, CoverageFormat, FileDiff};

pub fn warn_if_legacy_flag_used(alias: &str, canonical: &str) {
    if !deprecation_warnings_enabled() || global_quiet_enabled() || global_json_enabled() {
        return;
    }
    let used = std::env::args().any(|arg| arg == alias);
    if used {
        eprintln!(
            "warning: `{alias}` is deprecated; use `{canonical}` (to be removed after 2 minor releases)."
        );
    }
}

pub fn global_quiet_enabled() -> bool {
    std::env::args().any(|arg| arg == "-q" || arg == "--quiet")
}

pub fn global_json_enabled() -> bool {
    std::env::args().any(|arg| arg == "--json")
}

pub fn deprecation_warnings_enabled() -> bool {
    match std::env::var("COVY_DEPRECATION_WARNINGS") {
        Ok(v) => {
            let normalized = v.trim().to_ascii_lowercase();
            normalized == "1" || normalized == "true" || normalized == "yes" || normalized == "on"
        }
        Err(_) => false,
    }
}

pub fn maybe_warn_deprecated(message: &str) {
    if deprecation_warnings_enabled() && !global_quiet_enabled() && !global_json_enabled() {
        eprintln!("{message}");
    }
}

pub fn deserialize_json_with_example<T: serde::de::DeserializeOwned>(
    input: &str,
    type_name: &str,
    example: &str,
) -> anyhow::Result<T> {
    serde_json::from_str(input).map_err(|e| {
        anyhow::anyhow!("Failed to parse {type_name}: {e}\n\nExpected JSON shape:\n{example}")
    })
}

pub fn default_impact_adapters() -> testy_core::pipeline::ImpactAdapters {
    testy_core::pipeline::ImpactAdapters {
        ingest_coverage_auto,
        ingest_coverage_with_format,
        git_diff: impact_git_diff,
    }
}

pub fn default_testmap_adapters() -> testy_core::pipeline_testmap::TestMapAdapters {
    testy_core::pipeline_testmap::TestMapAdapters {
        ingest_coverage: ingest_coverage_auto,
    }
}

fn ingest_coverage_auto(path: &Path) -> Result<CoverageData> {
    covy_ingest::ingest_path(path).map_err(Into::into)
}

fn ingest_coverage_with_format(path: &Path, format: CoverageFormat) -> Result<CoverageData> {
    covy_ingest::ingest_path_with_format(path, format).map_err(Into::into)
}

fn impact_git_diff(base: &str, head: &str) -> Result<Vec<FileDiff>> {
    diffy_core::diff::git_diff(base, head).map_err(Into::into)
}
