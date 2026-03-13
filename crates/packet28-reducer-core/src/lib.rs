mod read;
mod search;
#[cfg(test)]
mod tests;
mod types;

pub use read::read_regions;
pub use search::{
    format_region, infer_symbols_from_lines, infer_symbols_from_pattern, normalize_capture_path,
    parse_region_for_path, search,
};
#[cfg(test)]
pub(crate) use search::{parse_grep_output_line, render_search_compact_preview};
pub use types::*;
