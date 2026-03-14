use std::path::Path;

use crate::{
    normalize_capture_path, parse_grep_output_line, render_search_compact_preview, SearchGroup,
    SearchMatch,
};

#[test]
fn normalize_absolute_path_strips_root() {
    let root = Path::new("/tmp/example");
    let path = normalize_capture_path(root, "/tmp/example/src/lib.rs");
    assert_eq!(path, "src/lib.rs");
}

#[test]
fn compact_preview_mentions_groups() {
    let preview = render_search_compact_preview(
        3,
        &[SearchGroup {
            path: "src/lib.rs".to_string(),
            match_count: 3,
            displayed_match_count: 2,
            truncated: true,
            matches: vec![
                SearchMatch {
                    path: "src/lib.rs".to_string(),
                    line: 4,
                    text: "alpha".to_string(),
                },
                SearchMatch {
                    path: "src/lib.rs".to_string(),
                    line: 8,
                    text: "beta".to_string(),
                },
            ],
        }],
        50,
    );
    assert!(preview.contains("Search found 3 matches in 1 files."));
    assert!(preview.contains("src/lib.rs"));
    assert!(!preview.contains("alpha"));
}

#[test]
fn parse_grep_output_line_accepts_grep_h_output_for_single_file() {
    let root = Path::new("/tmp/example");
    let parsed =
        parse_grep_output_line(root, "src/lib.rs:41:pub struct Alpha;", Some("src/lib.rs"))
            .expect("single-file grep -H output should parse");
    assert_eq!(parsed.0, "src/lib.rs");
    assert_eq!(parsed.1, 41);
    assert_eq!(parsed.2, "pub struct Alpha;");
}

#[test]
fn parse_grep_output_line_accepts_rg_single_file_output() {
    let root = Path::new("/tmp/example");
    let parsed = parse_grep_output_line(root, "41:pub struct Alpha;", Some("src/lib.rs"))
        .expect("single-file rg output should parse");
    assert_eq!(parsed.0, "src/lib.rs");
    assert_eq!(parsed.1, 41);
    assert_eq!(parsed.2, "pub struct Alpha;");
}
