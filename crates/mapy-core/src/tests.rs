use std::collections::BTreeSet;

use super::*;

#[test]
fn deterministic_tie_breaks_are_lexical() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.rs"), "fn alpha() {}\n").unwrap();
    std::fs::write(root.join("src/b.rs"), "fn beta() {}\n").unwrap();

    let env = build_repo_map(RepoMapRequest {
        repo_root: root.to_string_lossy().to_string(),
        max_files: 10,
        max_symbols: 10,
        ..RepoMapRequest::default()
    })
    .unwrap();

    assert!(!env.payload.files_ranked.is_empty());
    let left = env
        .files
        .get(env.payload.files_ranked[0].file_idx)
        .map(|f| f.path.clone())
        .unwrap_or_default();
    let right = env
        .files
        .get(env.payload.files_ranked[1].file_idx)
        .map(|f| f.path.clone())
        .unwrap_or_default();
    assert!(left <= right);
}

#[test]
fn excludes_generated_paths_by_default() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("src/main/java/com/example")).unwrap();
    std::fs::create_dir_all(root.join("target/site/jacoco/jacoco-resources")).unwrap();

    std::fs::write(
        root.join("src/main/java/com/example/Calculator.java"),
        "public class Calculator { public int add(int a, int b) { return a + b; } }",
    )
    .unwrap();
    std::fs::write(
        root.join("target/site/jacoco/jacoco-resources/prettify.js"),
        "function prettyPrint() {}",
    )
    .unwrap();

    let env = build_repo_map(RepoMapRequest {
        repo_root: root.to_string_lossy().to_string(),
        ..RepoMapRequest::default()
    })
    .unwrap();

    assert!(env.files.iter().all(|f| !f.path.contains("target/")));
}

#[test]
fn extracts_java_symbols_with_modifiers() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("src/main/java/com/example")).unwrap();
    std::fs::write(
        root.join("src/main/java/com/example/Calculator.java"),
        r#"
package com.example;

public class Calculator {
  public int add(int a, int b) { return a + b; }
  private static String label() { return "x"; }
}
"#,
    )
    .unwrap();

    let env = build_repo_map(RepoMapRequest {
        repo_root: root.to_string_lossy().to_string(),
        ..RepoMapRequest::default()
    })
    .unwrap();

    let names = env
        .symbols
        .iter()
        .map(|s| s.name.as_str())
        .collect::<BTreeSet<_>>();
    assert!(names.contains("Calculator"));
    assert!(names.contains("add"));
    assert!(names.contains("label"));
}

#[test]
fn extracts_java_import_edges_from_ast() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("src/main/java/com/example")).unwrap();
    std::fs::write(
        root.join("src/main/java/com/example/Util.java"),
        "package com.example; public class Util {}",
    )
    .unwrap();
    std::fs::write(
        root.join("src/main/java/com/example/Calculator.java"),
        r#"
package com.example;
import com.example.Util;
public class Calculator {
  public int add(int a, int b) { return a + b; }
}
"#,
    )
    .unwrap();

    let env = build_repo_map(RepoMapRequest {
        repo_root: root.to_string_lossy().to_string(),
        ..RepoMapRequest::default()
    })
    .unwrap();

    assert!(
        env.payload.edges.iter().any(|edge| {
            let from = env
                .files
                .get(edge.from_file_idx)
                .map(|f| f.path.as_str())
                .unwrap_or("");
            let to = env
                .files
                .get(edge.to_file_idx)
                .map(|f| f.path.as_str())
                .unwrap_or("");
            from.ends_with("Calculator.java") && to.ends_with("Util.java")
        }),
        "expected import edge from Calculator.java to Util.java"
    );
}

#[test]
fn extracts_import_leaves_without_mangling_common_syntaxes() {
    let python = r#"
import importlib
from pathlib import Path
"#;
    let (_, python_imports) =
        extract_metadata_ast_with_lines(SourceLanguage::Python, python).unwrap();
    let python_imports = python_imports.into_iter().collect::<BTreeSet<_>>();
    assert!(python_imports.contains("importlib"));
    assert!(python_imports.contains("pathlib"));

    let javascript = r#"
import { foo } from "pkg";
"#;
    let (_, javascript_imports) =
        extract_metadata_ast_with_lines(SourceLanguage::JavaScript, javascript).unwrap();
    assert_eq!(javascript_imports, vec!["pkg".to_string()]);

    let java = r#"
import com.example.Util;
import static com.example.Util.parse;
"#;
    let (_, java_imports) = extract_metadata_ast_with_lines(SourceLanguage::Java, java).unwrap();
    let java_imports = java_imports.into_iter().collect::<BTreeSet<_>>();
    assert!(java_imports.contains("com.example.Util"));
    assert!(!java_imports.contains("parse"));

    let regex_imports = crate::scan::extract_imports(
        r#"
import "./util.ts";
#include <foo/bar.hpp>
import static com.example.Util.parse;
"#,
    )
    .into_iter()
    .collect::<BTreeSet<_>>();
    assert!(regex_imports.contains("./util"));
    assert!(regex_imports.contains("foo/bar"));
    assert!(regex_imports.contains("com.example.Util"));
}

#[test]
fn writes_incremental_cache_file() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "fn hello() {}\n").unwrap();

    build_repo_map(RepoMapRequest {
        repo_root: root.to_string_lossy().to_string(),
        ..RepoMapRequest::default()
    })
    .unwrap();

    assert!(root.join(".packet28/mapy-cache-v1.bin").exists());
}

#[test]
fn classifies_top_level_and_windows_test_paths() {
    assert!(crate::scan::is_test_path("tests/foo.rs"));
    assert!(crate::scan::is_test_path("test/helpers.py"));
    assert!(crate::scan::is_test_path(r"tests\foo.rs"));
    assert!(!crate::scan::is_test_path("src/tests_support.rs"));
}

#[test]
fn excludes_hidden_tmp_paths_from_scan() {
    assert!(crate::scan::is_generated_or_vendor_path(
        ".tmp-rtk-reference/src/lib.rs"
    ));
    assert!(crate::scan::is_generated_or_vendor_path("foo/.tmp/bar.rs"));
    assert!(!crate::scan::is_generated_or_vendor_path(
        "src/tmp_helper.rs"
    ));
}

#[test]
fn extracts_symbols_for_non_java_languages() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();

    std::fs::write(
        root.join("src/lib.rs"),
        "fn parse_input() {}\nstruct Engine;\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/main.py"),
        "class Parser:\n  pass\n\ndef parse_input():\n  return 1\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/app.ts"),
        "interface Runner {}\nfunction parseInput() { return 1 }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/app.js"),
        "class Handler {}\nfunction handleInput() { return 1 }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/main.go"),
        "package main\nimport \"fmt\"\nfunc ParseInput() {}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/main.cpp"),
        "#include <vector>\nclass Parser{};\nint parse_input(){ return 0; }\n",
    )
    .unwrap();

    let env = build_repo_map(RepoMapRequest {
        repo_root: root.to_string_lossy().to_string(),
        ..RepoMapRequest::default()
    })
    .unwrap();

    let names = env
        .symbols
        .iter()
        .map(|s| s.name.clone())
        .collect::<BTreeSet<_>>();
    assert!(names.contains("parse_input") || names.contains("ParseInput"));
    assert!(names.contains("Engine"));
    assert!(names.contains("Parser") || names.contains("Handler"));
}

#[test]
fn focus_symbols_boost_matching_crate_paths_and_attach_symbol_files() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("crates/diffy-core/src")).unwrap();
    std::fs::create_dir_all(root.join("crates/testy-core/src")).unwrap();
    std::fs::write(
        root.join("crates/diffy-core/src/lib.rs"),
        "pub fn analyze_diffy() {}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("crates/testy-core/src/lib.rs"),
        "pub fn analyze_tests() {}\n",
    )
    .unwrap();

    let env = build_repo_map(RepoMapRequest {
        repo_root: root.to_string_lossy().to_string(),
        focus_symbols: vec!["diffy".to_string()],
        max_files: 4,
        max_symbols: 8,
        ..RepoMapRequest::default()
    })
    .unwrap();

    let top_file = env
        .payload
        .files_ranked
        .first()
        .and_then(|ranked| env.files.get(ranked.file_idx))
        .map(|file| file.path.clone())
        .unwrap_or_default();
    assert!(
        top_file.contains("diffy-core"),
        "expected diffy crate to outrank unrelated files, got {top_file}"
    );
    assert!(env.symbols.iter().any(|symbol| symbol
        .file
        .as_deref()
        .is_some_and(|file| file.contains("diffy-core"))));
}

#[test]
fn resolves_rust_use_edges_with_module_paths() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("crates/sample/src/foo")).unwrap();
    std::fs::write(root.join("crates/sample/src/lib.rs"), "pub mod foo;\n").unwrap();
    std::fs::write(root.join("crates/sample/src/foo/mod.rs"), "pub mod util;\n").unwrap();
    std::fs::write(
        root.join("crates/sample/src/foo/util.rs"),
        "pub fn helper() {}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("crates/sample/src/foo/worker.rs"),
        "use super::util::helper;\npub fn run() { helper(); }\n",
    )
    .unwrap();

    let env = build_repo_map(RepoMapRequest {
        repo_root: root.to_string_lossy().to_string(),
        ..RepoMapRequest::default()
    })
    .unwrap();

    assert!(env.payload.edges.iter().any(|edge| {
        let from = env
            .files
            .get(edge.from_file_idx)
            .map(|file| file.path.as_str())
            .unwrap_or("");
        let to = env
            .files
            .get(edge.to_file_idx)
            .map(|file| file.path.as_str())
            .unwrap_or("");
        from.ends_with("worker.rs") && to.ends_with("util.rs")
    }));
}

#[test]
fn resolves_relative_typescript_imports_to_local_modules() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src/widgets")).unwrap();
    std::fs::write(
        root.join("src/widgets/util.ts"),
        "export const helper = () => 1;\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/widgets/index.ts"),
        "import { helper } from \"./util\";\nexport const run = () => helper();\n",
    )
    .unwrap();

    let env = build_repo_map(RepoMapRequest {
        repo_root: root.to_string_lossy().to_string(),
        ..RepoMapRequest::default()
    })
    .unwrap();

    assert!(env.payload.edges.iter().any(|edge| {
        let from = env
            .files
            .get(edge.from_file_idx)
            .map(|file| file.path.as_str())
            .unwrap_or("");
        let to = env
            .files
            .get(edge.to_file_idx)
            .map(|file| file.path.as_str())
            .unwrap_or("");
        from.ends_with("index.ts") && to.ends_with("util.ts")
    }));
}

#[test]
fn extracts_callable_variable_symbols_for_js_and_ts() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/app.ts"),
        "export const boot = () => 1;\nconst Widget = class Widget {};\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/app.js"),
        "const handle = function () { return 1; };\nconst Service = class Service {};\n",
    )
    .unwrap();

    let env = build_repo_map(RepoMapRequest {
        repo_root: root.to_string_lossy().to_string(),
        ..RepoMapRequest::default()
    })
    .unwrap();

    let names = env
        .symbols
        .iter()
        .map(|symbol| symbol.name.as_str())
        .collect::<BTreeSet<_>>();
    assert!(names.contains("boot"));
    assert!(names.contains("Widget"));
    assert!(names.contains("handle"));
    assert!(names.contains("Service"));
}

#[test]
fn build_repo_index_captures_symbol_lines_and_token_regions() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/Sample.java"),
        "class Sample {\n  void isBlank() {}\n  void demo() { isBlank(); }\n}\n",
    )
    .unwrap();

    let snapshot = build_repo_index(root, true).unwrap();
    let file = snapshot.files.get("src/Sample.java").unwrap();
    assert!(file
        .symbols
        .iter()
        .any(|symbol| { symbol.name == "isBlank" && symbol.kind == "method" && symbol.line == 2 }));
    assert_eq!(file.token_lines.get("isblank").cloned(), Some(vec![2, 3]));
}

#[test]
fn repo_query_exact_symbol_returns_single_match() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn build_repo_map() {}\n").unwrap();
    std::fs::write(root.join("src/other.rs"), "pub fn build_repo_mapper() {}\n").unwrap();

    let envelope = build_repo_query(RepoQueryRequest {
        repo_root: root.to_string_lossy().to_string(),
        symbol_query: "build_repo_map".to_string(),
        exact: true,
        max_results: 5,
        ..RepoQueryRequest::default()
    })
    .unwrap();

    let rich = expand_repo_query_payload(&envelope);
    assert_eq!(rich.matches.len(), 1);
    assert_eq!(rich.matches[0].file, "src/lib.rs");
    assert_eq!(rich.matches[0].symbol, "build_repo_map");
    assert_eq!(rich.matches[0].line, 1);
}

#[test]
fn repo_query_files_only_dedupes_multiple_symbol_hits_per_file() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn build_repo_map() {}\npub fn build_repo_map_extra() {}\n",
    )
    .unwrap();

    let envelope = build_repo_query(RepoQueryRequest {
        repo_root: root.to_string_lossy().to_string(),
        symbol_query: "build_repo_map".to_string(),
        files_only: true,
        max_results: 5,
        ..RepoQueryRequest::default()
    })
    .unwrap();

    assert_eq!(envelope.payload.matches.len(), 1);
    assert_eq!(envelope.files.len(), 1);
    assert_eq!(envelope.files[0].path, "src/lib.rs");
}

#[test]
fn repo_query_uses_cache_for_warm_symbol_lookup() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn should_tee() {}\n").unwrap();

    build_repo_map(RepoMapRequest {
        repo_root: root.to_string_lossy().to_string(),
        ..RepoMapRequest::default()
    })
    .unwrap();

    let envelope = build_repo_query(RepoQueryRequest {
        repo_root: root.to_string_lossy().to_string(),
        symbol_query: "should_tee".to_string(),
        exact: true,
        max_results: 5,
        ..RepoQueryRequest::default()
    })
    .unwrap();

    assert_eq!(envelope.files[0].path, "src/lib.rs");
}

#[test]
fn repo_query_pattern_matches_rust_function_definition() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn should_tee(value: bool) -> bool {\n    value\n}\n",
    )
    .unwrap();

    let envelope = build_repo_query(RepoQueryRequest {
        repo_root: root.to_string_lossy().to_string(),
        pattern_query: "fn should_tee($$$)".to_string(),
        language: "rust".to_string(),
        max_results: 5,
        ..RepoQueryRequest::default()
    })
    .unwrap();

    let rich = expand_repo_query_payload(&envelope);
    assert_eq!(rich.matches.len(), 1);
    assert_eq!(rich.matches[0].file, "src/lib.rs");
    assert_eq!(rich.matches[0].symbol, "should_tee");
    assert_eq!(rich.matches[0].line, 1);
}

#[test]
fn repo_query_pattern_files_only_dedupes_matches_per_file() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();

    let envelope = build_repo_query(RepoQueryRequest {
        repo_root: root.to_string_lossy().to_string(),
        pattern_query: "fn $NAME($$$)".to_string(),
        language: "rust".to_string(),
        selector: "function_item".to_string(),
        files_only: true,
        max_results: 5,
        ..RepoQueryRequest::default()
    })
    .unwrap();

    assert_eq!(envelope.payload.matches.len(), 1);
    assert_eq!(envelope.files.len(), 1);
    assert_eq!(envelope.files[0].path, "src/lib.rs");
}

#[test]
fn update_repo_index_only_touches_changed_files() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.rs"), "fn alpha() {}\n").unwrap();
    std::fs::write(root.join("src/b.rs"), "fn beta() {}\n").unwrap();

    let mut snapshot = build_repo_index(root, true).unwrap();
    let original_beta = snapshot.files.get("src/b.rs").cloned().unwrap();

    std::fs::write(root.join("src/a.rs"), "fn alpha() {}\nfn gamma() {}\n").unwrap();
    let summary = update_repo_index(root, &mut snapshot, &["src/a.rs".to_string()], true).unwrap();

    assert_eq!(summary.changed_paths, vec!["src/a.rs".to_string()]);
    assert_eq!(summary.indexed_files, 1);
    assert_eq!(
        snapshot.files.get("src/b.rs").cloned().unwrap(),
        original_beta
    );
    assert!(snapshot
        .files
        .get("src/a.rs")
        .is_some_and(|file| file.symbols.iter().any(|symbol| symbol.name == "gamma")));
}

#[test]
fn focus_term_match_score_graduates_exact_and_partial_matches() {
    assert_eq!(focus_term_match_score("shuffle", "shuffle"), 1.0);
    assert_eq!(focus_term_match_score("shuffleConfig", "shuffle"), 0.6);
    assert_eq!(focus_term_match_score("assemble", "assembly"), 0.45);
    assert_eq!(focus_term_match_score("ArrayUtils", "shuffle"), 0.0);
}

#[test]
fn file_focus_match_prefers_exact_symbol_matches_over_path_only_matches() {
    let symbols = vec![("method".to_string(), "shuffle".to_string())];
    let focus_paths = Vec::new();
    let focus_symbols = BTreeSet::from(["shuffle".to_string()]);

    let direct = file_focus_match(
        "src/main/java/org/apache/commons/lang3/ArrayUtils.java",
        &symbols,
        &focus_paths,
        &focus_symbols,
    );
    let indirect = file_focus_match(
        "src/main/java/org/apache/commons/lang3/StringUtils.java",
        &[],
        &focus_paths,
        &focus_symbols,
    );

    assert!(direct > indirect);
    assert_eq!(direct, 1.0);
    assert_eq!(indirect, 0.0);
}

#[test]
fn file_focus_match_rewards_multiple_path_term_hits() {
    let focus_paths = Vec::new();
    let focus_symbols = BTreeSet::from(["assemble".to_string(), "contextq".to_string()]);

    let multi = file_focus_match(
        "crates/contextq-core/src/assemble.rs",
        &[],
        &focus_paths,
        &focus_symbols,
    );
    let single_path = file_focus_match(
        "crates/contextq-core/src/lib.rs",
        &[],
        &focus_paths,
        &focus_symbols,
    );
    let single_file = file_focus_match(
        "crates/suite-packet-core/src/assemble.rs",
        &[],
        &focus_paths,
        &focus_symbols,
    );

    assert!(multi > single_path);
    assert!(multi > single_file);
    assert_eq!(single_path, 0.3);
    assert_eq!(single_file, 0.3);
}

#[test]
fn build_repo_map_prefers_stem_close_multi_term_paths() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("crates/contextq-core/src")).unwrap();
    std::fs::create_dir_all(root.join("crates/assembly-core/src")).unwrap();
    std::fs::write(root.join("crates/contextq-core/src/assemble.rs"), "pub fn run() {}\n").unwrap();
    std::fs::write(root.join("crates/contextq-core/src/lib.rs"), "pub fn lib() {}\n").unwrap();
    std::fs::write(root.join("crates/assembly-core/src/lib.rs"), "pub fn lib() {}\n").unwrap();

    let env = build_repo_map(RepoMapRequest {
        repo_root: root.to_string_lossy().to_string(),
        focus_symbols: vec!["assembly".to_string(), "contextq".to_string()],
        max_files: 3,
        max_symbols: 8,
        ..RepoMapRequest::default()
    })
    .unwrap();

    let ranked_paths = env
        .payload
        .files_ranked
        .iter()
        .filter_map(|ranked| env.files.get(ranked.file_idx))
        .map(|file| file.path.clone())
        .collect::<Vec<_>>();

    assert_eq!(
        ranked_paths.first().map(String::as_str),
        Some("crates/contextq-core/src/assemble.rs")
    );
}
