use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{
    normalize_capture_path, parse_grep_output_line, render_search_compact_preview,
    search_with_test_rg, search_without_rg, SearchGroup, SearchMatch, SearchRequest,
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
    assert!(preview.contains("src/lib.rs:4:alpha"));
    assert!(preview.contains("src/lib.rs:8:beta"));
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

#[test]
fn reducer_fallback_matches_anchored_line_start_regexes_without_rg() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "packet28-reducer-core-search-test-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(root.join("src")).expect("create test fixture");
    fs::write(
        root.join("src/main.rs"),
        "fn build() {\n    SearchRequest {\n        query: pattern,\n    };\n}\n",
    )
    .expect("write test fixture");

    let request = SearchRequest {
        query: r"^\s*SearchRequest\s*\{".to_string(),
        ..SearchRequest::default()
    };
    let result = search_without_rg(&root, &request).expect("fallback search should succeed");

    assert_eq!(result.match_count, 1);
    assert_eq!(result.paths, vec!["src/main.rs".to_string()]);
    assert_eq!(result.groups[0].matches[0].line, 2);
    assert_eq!(result.groups[0].matches[0].text, "    SearchRequest {");

    fs::remove_dir_all(&root).expect("cleanup test fixture");
}

/// A private fixture root with a stub `rg` that prints `stderr` and exits
/// with `status`, so tests do not depend on the host's ripgrep or `PATH`.
#[cfg(unix)]
struct StubRg {
    root: PathBuf,
    binary: PathBuf,
}

#[cfg(unix)]
impl StubRg {
    fn new(name: &str, stderr: &[u8], status: i32) -> Self {
        use std::os::unix::fs::PermissionsExt as _;

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "packet28-reducer-core-{name}-{}-{unique}",
            std::process::id()
        ));
        let bin = root.join("bin");
        fs::create_dir_all(&bin).expect("create stub rg directory");
        fs::write(bin.join("stderr.txt"), stderr).expect("write stub stderr");
        let binary = bin.join("rg");
        fs::write(
            &binary,
            format!("#!/bin/sh\ncat \"$(dirname \"$0\")/stderr.txt\" >&2\nexit {status}\n"),
        )
        .expect("write stub rg");
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))
            .expect("make stub rg executable");
        Self { root, binary }
    }

    fn search(&self, query: &str) -> anyhow::Result<crate::SearchResult> {
        let request = SearchRequest {
            query: query.to_string(),
            ..SearchRequest::default()
        };
        search_with_test_rg(&self.root, &request, &self.binary)
    }
}

#[cfg(unix)]
impl Drop for StubRg {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[cfg(unix)]
#[test]
fn failed_search_error_includes_child_status_and_stderr() {
    let stub = StubRg::new(
        "search-parse-error",
        b"rg: regex parse error:\n    (?:[)\n       ^\nerror: unclosed character class\n",
        2,
    );

    let error = stub.search("[").unwrap_err().to_string();

    assert_eq!(
        error,
        "search command exited with status exit status: 2; stderr:\n\
         rg: regex parse error:\n    (?:[)\n       ^\nerror: unclosed character class"
    );
}

#[cfg(unix)]
#[test]
fn failed_search_error_truncates_oversized_stderr_at_a_char_boundary() {
    // 1 ASCII byte then 2-byte characters: byte 2048 falls inside a character.
    let stderr = format!("x{}", "\u{e9}".repeat(2000));
    let stub = StubRg::new("search-oversized-stderr", stderr.as_bytes(), 2);

    let error = stub.search("[").unwrap_err().to_string();

    let expected = format!(
        "search command exited with status exit status: 2; stderr:\n{}\n\
         [stderr truncated to 2047 of 4001 bytes]",
        &stderr[..2047]
    );
    assert_eq!(error, expected);
}

#[cfg(unix)]
#[test]
fn no_match_search_status_still_returns_empty_result_with_diagnostics() {
    let stub = StubRg::new("search-no-match", b"rg: ./missing: No such file\n", 1);

    let result = stub.search("absent").expect("status 1 means no match");

    assert_eq!(
        (result.match_count, result.diagnostics),
        (0, vec!["rg: ./missing: No such file".to_string()])
    );
}
