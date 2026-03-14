use super::*;

use std::cell::RefCell;
use std::collections::BTreeSet;

use regex::Regex;
use tree_sitter::{Node, Parser};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceLanguage {
    Java,
    Rust,
    Python,
    TypeScript,
    TypeScriptJsx,
    JavaScript,
    Go,
    Cpp,
}

pub(crate) fn detect_source_language(path: &str) -> Option<SourceLanguage> {
    if path.ends_with(".java") {
        return Some(SourceLanguage::Java);
    }
    if path.ends_with(".rs") {
        return Some(SourceLanguage::Rust);
    }
    if path.ends_with(".py") {
        return Some(SourceLanguage::Python);
    }
    if path.ends_with(".tsx") {
        return Some(SourceLanguage::TypeScriptJsx);
    }
    if path.ends_with(".ts") {
        return Some(SourceLanguage::TypeScript);
    }
    if path.ends_with(".js") || path.ends_with(".jsx") {
        return Some(SourceLanguage::JavaScript);
    }
    if path.ends_with(".go") {
        return Some(SourceLanguage::Go);
    }
    if path.ends_with(".cpp")
        || path.ends_with(".cc")
        || path.ends_with(".cxx")
        || path.ends_with(".hpp")
        || path.ends_with(".hh")
        || path.ends_with(".h")
        || path.ends_with(".c")
    {
        return Some(SourceLanguage::Cpp);
    }
    None
}

pub(crate) fn symbol_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?m)^\s*(?:(?P<kind>fn|struct|enum|trait|impl|class|interface|def|function)\s+)(?P<name>[A-Za-z_][A-Za-z0-9_]*)",
        )
        .expect("valid symbol regex")
    })
}

pub(crate) fn java_type_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?m)^\s*(?:public|protected|private|abstract|static|final|sealed|non-sealed|\s)*\b(?P<kind>class|interface|enum|record)\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)",
        )
        .expect("valid java type regex")
    })
}

pub(crate) fn java_method_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?m)^\s*(?:public|protected|private|static|final|abstract|synchronized|native|strictfp|\s)+(?:<[^>]+>\s*)?(?:[A-Za-z_][A-Za-z0-9_<>\[\],.?]*\s+)+(?P<name>[A-Za-z_][A-Za-z0-9_]*)\s*\([^;\n{}]*\)\s*(?:\{|throws\b)",
        )
        .expect("valid java method regex")
    })
}

pub(crate) fn identifier_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?P<token>[A-Za-z_][A-Za-z0-9_]*)").expect("valid identifier regex")
    })
}

thread_local! {
    static JAVA_PARSER: RefCell<Option<Parser>> = RefCell::new(init_java_parser());
    static RUST_PARSER: RefCell<Option<Parser>> = RefCell::new(init_rust_parser());
    static PYTHON_PARSER: RefCell<Option<Parser>> = RefCell::new(init_python_parser());
    static TYPESCRIPT_PARSER: RefCell<Option<Parser>> = RefCell::new(init_typescript_parser());
    static TSX_PARSER: RefCell<Option<Parser>> = RefCell::new(init_tsx_parser());
    static JAVASCRIPT_PARSER: RefCell<Option<Parser>> = RefCell::new(init_javascript_parser());
    static GO_PARSER: RefCell<Option<Parser>> = RefCell::new(init_go_parser());
    static CPP_PARSER: RefCell<Option<Parser>> = RefCell::new(init_cpp_parser());
}

fn init_java_parser() -> Option<Parser> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_java::LANGUAGE.into())
        .ok()?;
    Some(parser)
}

fn init_rust_parser() -> Option<Parser> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .ok()?;
    Some(parser)
}

fn init_python_parser() -> Option<Parser> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .ok()?;
    Some(parser)
}

fn init_typescript_parser() -> Option<Parser> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
        .ok()?;
    Some(parser)
}

fn init_tsx_parser() -> Option<Parser> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_typescript::LANGUAGE_TSX.into())
        .ok()?;
    Some(parser)
}

fn init_javascript_parser() -> Option<Parser> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_javascript::LANGUAGE.into())
        .ok()?;
    Some(parser)
}

fn init_go_parser() -> Option<Parser> {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_go::LANGUAGE.into()).ok()?;
    Some(parser)
}

fn init_cpp_parser() -> Option<Parser> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_cpp::LANGUAGE.into())
        .ok()?;
    Some(parser)
}

pub(crate) fn extract_metadata_ast_with_lines(
    language: SourceLanguage,
    content: &str,
) -> Option<(Vec<IndexedSymbolDef>, Vec<String>)> {
    match language {
        SourceLanguage::Java => extract_java_metadata_ast(content),
        SourceLanguage::Rust => extract_rust_metadata_ast(content),
        SourceLanguage::Python => extract_python_metadata_ast(content),
        SourceLanguage::TypeScript => extract_typescript_metadata_ast(content),
        SourceLanguage::TypeScriptJsx => extract_tsx_metadata_ast(content),
        SourceLanguage::JavaScript => extract_javascript_metadata_ast(content),
        SourceLanguage::Go => extract_go_metadata_ast(content),
        SourceLanguage::Cpp => extract_cpp_metadata_ast(content),
    }
}

fn extract_java_metadata_ast(content: &str) -> Option<(Vec<IndexedSymbolDef>, Vec<String>)> {
    JAVA_PARSER.with(|cell| {
        let mut parser = cell.borrow_mut();
        let parser = parser.as_mut()?;
        let tree = parser.parse(content, None)?;

        let mut symbols = BTreeSet::<IndexedSymbolDef>::new();
        let mut imports = BTreeSet::<String>::new();
        walk_java_ast(
            tree.root_node(),
            content.as_bytes(),
            &mut symbols,
            &mut imports,
        );
        Some((symbols.into_iter().collect(), imports.into_iter().collect()))
    })
}

fn extract_rust_metadata_ast(content: &str) -> Option<(Vec<IndexedSymbolDef>, Vec<String>)> {
    RUST_PARSER.with(|cell| extract_with_walker(cell, content, walk_rust_ast))
}

fn extract_python_metadata_ast(content: &str) -> Option<(Vec<IndexedSymbolDef>, Vec<String>)> {
    PYTHON_PARSER.with(|cell| extract_with_walker(cell, content, walk_python_ast))
}

fn extract_typescript_metadata_ast(content: &str) -> Option<(Vec<IndexedSymbolDef>, Vec<String>)> {
    TYPESCRIPT_PARSER.with(|cell| extract_with_walker(cell, content, walk_typescript_ast))
}

fn extract_tsx_metadata_ast(content: &str) -> Option<(Vec<IndexedSymbolDef>, Vec<String>)> {
    TSX_PARSER.with(|cell| extract_with_walker(cell, content, walk_typescript_ast))
}

fn extract_javascript_metadata_ast(content: &str) -> Option<(Vec<IndexedSymbolDef>, Vec<String>)> {
    JAVASCRIPT_PARSER.with(|cell| extract_with_walker(cell, content, walk_javascript_ast))
}

fn extract_go_metadata_ast(content: &str) -> Option<(Vec<IndexedSymbolDef>, Vec<String>)> {
    GO_PARSER.with(|cell| extract_with_walker(cell, content, walk_go_ast))
}

fn extract_cpp_metadata_ast(content: &str) -> Option<(Vec<IndexedSymbolDef>, Vec<String>)> {
    CPP_PARSER.with(|cell| extract_with_walker(cell, content, walk_cpp_ast))
}

fn extract_with_walker(
    cell: &RefCell<Option<Parser>>,
    content: &str,
    walker: fn(Node<'_>, &[u8], &mut BTreeSet<IndexedSymbolDef>, &mut BTreeSet<String>),
) -> Option<(Vec<IndexedSymbolDef>, Vec<String>)> {
    let mut parser = cell.borrow_mut();
    let parser = parser.as_mut()?;
    let tree = parser.parse(content, None)?;
    let mut symbols = BTreeSet::<IndexedSymbolDef>::new();
    let mut imports = BTreeSet::<String>::new();
    walker(
        tree.root_node(),
        content.as_bytes(),
        &mut symbols,
        &mut imports,
    );
    Some((symbols.into_iter().collect(), imports.into_iter().collect()))
}

fn walk_java_ast(
    node: Node<'_>,
    src: &[u8],
    symbols: &mut BTreeSet<IndexedSymbolDef>,
    imports: &mut BTreeSet<String>,
) {
    match node.kind() {
        "class_declaration" => insert_named_child(node, src, "class", symbols),
        "interface_declaration" => insert_named_child(node, src, "interface", symbols),
        "enum_declaration" => insert_named_child(node, src, "enum", symbols),
        "record_declaration" => insert_named_child(node, src, "record", symbols),
        "method_declaration" => insert_named_child(node, src, "method", symbols),
        "constructor_declaration" => insert_named_child(node, src, "constructor", symbols),
        "import_declaration" => insert_java_import(node, src, imports),
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_java_ast(child, src, symbols, imports);
    }
}

fn walk_rust_ast(
    node: Node<'_>,
    src: &[u8],
    symbols: &mut BTreeSet<IndexedSymbolDef>,
    imports: &mut BTreeSet<String>,
) {
    match node.kind() {
        "function_item" => insert_name_or_identifier(node, src, "function", symbols),
        "struct_item" => insert_name_or_identifier(node, src, "struct", symbols),
        "enum_item" => insert_name_or_identifier(node, src, "enum", symbols),
        "trait_item" => insert_name_or_identifier(node, src, "trait", symbols),
        "type_item" => insert_name_or_identifier(node, src, "type", symbols),
        "use_declaration" => insert_import_leaf(node, src, imports),
        _ => {}
    }

    walk_children(node, src, symbols, imports, walk_rust_ast);
}

fn walk_python_ast(
    node: Node<'_>,
    src: &[u8],
    symbols: &mut BTreeSet<IndexedSymbolDef>,
    imports: &mut BTreeSet<String>,
) {
    match node.kind() {
        "function_definition" => insert_name_or_identifier(node, src, "function", symbols),
        "class_definition" => insert_name_or_identifier(node, src, "class", symbols),
        "import_statement" | "import_from_statement" => insert_import_leaf(node, src, imports),
        _ => {}
    }

    walk_children(node, src, symbols, imports, walk_python_ast);
}

fn walk_typescript_ast(
    node: Node<'_>,
    src: &[u8],
    symbols: &mut BTreeSet<IndexedSymbolDef>,
    imports: &mut BTreeSet<String>,
) {
    match node.kind() {
        "function_declaration" => insert_name_or_identifier(node, src, "function", symbols),
        "class_declaration" => insert_name_or_identifier(node, src, "class", symbols),
        "interface_declaration" => insert_name_or_identifier(node, src, "interface", symbols),
        "type_alias_declaration" => insert_name_or_identifier(node, src, "type", symbols),
        "enum_declaration" => insert_name_or_identifier(node, src, "enum", symbols),
        "method_definition" => insert_name_or_identifier(node, src, "method", symbols),
        "import_statement" => insert_import_leaf(node, src, imports),
        _ => {}
    }

    walk_children(node, src, symbols, imports, walk_typescript_ast);
}

fn walk_javascript_ast(
    node: Node<'_>,
    src: &[u8],
    symbols: &mut BTreeSet<IndexedSymbolDef>,
    imports: &mut BTreeSet<String>,
) {
    match node.kind() {
        "function_declaration" => insert_name_or_identifier(node, src, "function", symbols),
        "class_declaration" => insert_name_or_identifier(node, src, "class", symbols),
        "method_definition" => insert_name_or_identifier(node, src, "method", symbols),
        "import_statement" => insert_import_leaf(node, src, imports),
        _ => {}
    }

    walk_children(node, src, symbols, imports, walk_javascript_ast);
}

fn walk_go_ast(
    node: Node<'_>,
    src: &[u8],
    symbols: &mut BTreeSet<IndexedSymbolDef>,
    imports: &mut BTreeSet<String>,
) {
    match node.kind() {
        "function_declaration" => insert_name_or_identifier(node, src, "function", symbols),
        "method_declaration" => insert_name_or_identifier(node, src, "method", symbols),
        "type_spec" => insert_name_or_identifier(node, src, "type", symbols),
        "import_declaration" | "import_spec" => insert_import_leaf(node, src, imports),
        _ => {}
    }

    walk_children(node, src, symbols, imports, walk_go_ast);
}

fn walk_cpp_ast(
    node: Node<'_>,
    src: &[u8],
    symbols: &mut BTreeSet<IndexedSymbolDef>,
    imports: &mut BTreeSet<String>,
) {
    match node.kind() {
        "function_definition" => insert_name_or_identifier(node, src, "function", symbols),
        "class_specifier" => insert_name_or_identifier(node, src, "class", symbols),
        "struct_specifier" => insert_name_or_identifier(node, src, "struct", symbols),
        "enum_specifier" => insert_name_or_identifier(node, src, "enum", symbols),
        "preproc_include" => insert_import_leaf(node, src, imports),
        _ => {}
    }

    walk_children(node, src, symbols, imports, walk_cpp_ast);
}

fn walk_children(
    node: Node<'_>,
    src: &[u8],
    symbols: &mut BTreeSet<IndexedSymbolDef>,
    imports: &mut BTreeSet<String>,
    walker: fn(Node<'_>, &[u8], &mut BTreeSet<IndexedSymbolDef>, &mut BTreeSet<String>),
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walker(child, src, symbols, imports);
    }
}

fn insert_named_child(
    node: Node<'_>,
    src: &[u8],
    kind: &str,
    out: &mut BTreeSet<IndexedSymbolDef>,
) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let Ok(name) = name_node.utf8_text(src) else {
        return;
    };
    let name = name.trim();
    if !name.is_empty() && !is_reserved_word(name) {
        out.insert(IndexedSymbolDef {
            kind: kind.to_string(),
            name: name.to_string(),
            line: node.start_position().row + 1,
        });
    }
}

fn insert_name_or_identifier(
    node: Node<'_>,
    src: &[u8],
    kind: &str,
    out: &mut BTreeSet<IndexedSymbolDef>,
) {
    if let Some(name_node) = node.child_by_field_name("name") {
        if let Ok(name) = name_node.utf8_text(src) {
            let trimmed = name.trim();
            if !trimmed.is_empty() && !is_reserved_word(trimmed) {
                out.insert(IndexedSymbolDef {
                    kind: kind.to_string(),
                    name: trimmed.to_string(),
                    line: name_node.start_position().row + 1,
                });
                return;
            }
        }
    }

    if let Some(identifier) = find_identifier(node, src, 0) {
        if !identifier.is_empty() && !is_reserved_word(&identifier) {
            out.insert(IndexedSymbolDef {
                kind: kind.to_string(),
                name: identifier,
                line: node.start_position().row + 1,
            });
        }
    }
}

fn insert_java_import(node: Node<'_>, src: &[u8], out: &mut BTreeSet<String>) {
    let Ok(import_text) = node.utf8_text(src) else {
        return;
    };

    let mut normalized = normalize_import_candidate(import_text);
    if let Some(stripped) = normalized.strip_prefix("import") {
        normalized = stripped.trim().to_string();
    }
    let is_static = normalized.starts_with("static ");
    let candidate = normalized.strip_prefix("static ").unwrap_or(&normalized);

    if let Some(leaf) = resolve_java_import_leaf(candidate, is_static) {
        out.insert(leaf);
    }
}

fn insert_import_leaf(node: Node<'_>, src: &[u8], out: &mut BTreeSet<String>) {
    let Some(raw) = find_import_candidate(node, src) else {
        return;
    };
    if let Some(leaf) = resolve_import_leaf(&raw) {
        out.insert(leaf);
    }
}

pub(crate) fn normalize_import_candidate(raw: &str) -> String {
    raw.trim()
        .trim_end_matches(';')
        .trim_matches('"')
        .trim_matches('\'')
        .trim_matches('<')
        .trim_matches('>')
        .trim()
        .to_string()
}

pub(crate) fn resolve_java_import_leaf(raw: &str, is_static: bool) -> Option<String> {
    let normalized = normalize_import_candidate(raw);
    let trimmed = normalized.trim_end_matches(".*").trim();
    if trimmed.is_empty() {
        return None;
    }

    let segments = trimmed
        .split('.')
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    let leaf = if is_static {
        segments.iter().rev().nth(1).copied()
    } else {
        segments.last().copied()
    }?;

    if is_reserved_word(leaf) {
        None
    } else {
        Some(leaf.to_string())
    }
}

pub(crate) fn resolve_import_leaf(raw: &str) -> Option<String> {
    let normalized = normalize_import_candidate(raw);
    let trimmed = normalized.trim_end_matches(".*").trim();
    if trimmed.is_empty() {
        return None;
    }

    let leaf = if trimmed.contains('/') || trimmed.contains('\\') || trimmed.contains(':') {
        trimmed
            .rsplit(['/', '\\', ':'])
            .next()
            .unwrap_or(trimmed)
            .trim()
    } else {
        trimmed.rsplit('.').next().unwrap_or(trimmed).trim()
    };
    if leaf.is_empty() {
        return None;
    }

    let stem = leaf
        .rsplit_once('.')
        .map(|(base, ext)| {
            if !base.is_empty()
                && !ext.is_empty()
                && ext.chars().all(|ch| ch.is_ascii_alphanumeric())
            {
                base
            } else {
                leaf
            }
        })
        .unwrap_or(leaf)
        .trim();
    if stem.is_empty() || is_reserved_word(stem) {
        None
    } else {
        Some(stem.to_string())
    }
}

fn find_import_candidate(node: Node<'_>, src: &[u8]) -> Option<String> {
    for field in ["source", "module", "module_name", "path", "name"] {
        if let Some(child) = node.child_by_field_name(field) {
            if let Some(candidate) = find_import_candidate_in_subtree(child, src, 0) {
                return Some(candidate);
            }
        }
    }

    find_import_candidate_in_subtree(node, src, 0)
}

fn find_import_candidate_in_subtree(node: Node<'_>, src: &[u8], depth: usize) -> Option<String> {
    if depth > 6 {
        return None;
    }

    if let Some(score) = import_candidate_score(node.kind()) {
        let text = node.utf8_text(src).ok()?.trim().to_string();
        if !text.is_empty() {
            return Some((score, text)).map(|(_, text)| text);
        }
    }

    let mut best: Option<(u8, String)> = None;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named() {
            continue;
        }
        if let Some(candidate) = find_import_candidate_in_subtree(child, src, depth + 1) {
            let score = import_candidate_score(child.kind()).unwrap_or(3);
            let replace = best
                .as_ref()
                .map(|(best_score, _)| score < *best_score)
                .unwrap_or(true);
            if replace {
                best = Some((score, candidate));
            }
        }
    }

    best.map(|(_, candidate)| candidate)
}

fn import_candidate_score(kind: &str) -> Option<u8> {
    if matches!(
        kind,
        "string"
            | "string_literal"
            | "interpreted_string_literal"
            | "raw_string_literal"
            | "string_fragment"
            | "system_lib_string"
    ) {
        return Some(0);
    }
    if matches!(
        kind,
        "scoped_identifier"
            | "dotted_name"
            | "qualified_identifier"
            | "namespace_identifier"
            | "field_expression"
    ) {
        return Some(1);
    }
    if kind.contains("identifier") {
        return Some(2);
    }
    None
}

fn find_identifier(node: Node<'_>, src: &[u8], depth: usize) -> Option<String> {
    if depth > 5 {
        return None;
    }
    if node.kind() == "identifier" || node.kind() == "type_identifier" {
        let text = node.utf8_text(src).ok()?.trim().to_string();
        if !text.is_empty() {
            return Some(text);
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(found) = find_identifier(child, src, depth + 1) {
            return Some(found);
        }
    }
    None
}

pub(crate) fn is_reserved_word(name: &str) -> bool {
    matches!(
        name,
        "if" | "for" | "while" | "switch" | "catch" | "return" | "new" | "do" | "case"
    )
}

pub(crate) fn import_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?m)^\s*(?:use|from|#include|import(?:\s+static)?)\s+(?:<|"|')?(?P<target>[A-Za-z0-9_./:-]+)"#,
        )
            .expect("valid import regex")
    })
}
