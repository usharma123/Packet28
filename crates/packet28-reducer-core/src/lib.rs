mod command;
mod dotnet;
pub mod filter;
mod fs;
pub(crate) mod git;
mod github;
mod go;
mod infra;
mod javascript;
mod jvm;
pub mod parser;
mod python;
mod read;
mod ruby;
mod rust;
mod search;
pub mod tee;
#[cfg(test)]
mod tests;
mod types;

pub use command::{
    classify_command, classify_command_argv, consume_leading_env_assignments, reduce_command_output,
};
pub use git::compact_diff_public;
pub use read::read_regions;
pub use search::{
    format_region, infer_symbols_from_lines, infer_symbols_from_pattern, normalize_capture_path,
    parse_region_for_path, search,
};
#[cfg(test)]
pub(crate) use search::{
    parse_grep_output_line, render_search_compact_preview, search_with_test_rg, search_without_rg,
};
pub use types::*;

pub(crate) fn cache_fingerprint(family: &str, kind: &str, argv: &[String]) -> String {
    let material = format!("{family}:{kind}:{}", argv.join("\u{1f}"));
    let digest = blake3::hash(material.as_bytes()).to_hex().to_string();
    format!("{family}:{kind}:{digest}")
}
