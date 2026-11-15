#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use tree_sitter::{Language, Node, Parser};

const PPR_ITERATIONS: usize = 12;
const PPR_DAMPING: f64 = 0.85;

const TELEPORT_ACTIVE_FILE: f64 = 3.0;
const TELEPORT_DIRTY_FILE: f64 = 2.0;
const TELEPORT_QUERY_MATCH: f64 = 1.5;
const TELEPORT_BASELINE: f64 = 0.05;

const EDGE_IMPORT_FORWARD: f64 = 2.5;
const EDGE_IMPORT_REVERSE: f64 = 0.45;
const EDGE_SYMBOL_REVERSE_FACTOR: f64 = 0.20;

const MAX_SIGNATURES_PER_FILE: usize = 12;
const MAX_SIGNATURE_CHARS: usize = 160;
const MAX_SYMBOL_DEFINERS: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoFileInput {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoMapSeed {
    pub active_files: Vec<String>,
    pub dirty_files: Vec<String>,
    pub query_terms: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoSignature {
    pub name: String,
    pub kind: String,
    pub line: usize,
    pub signature: String,
    pub is_member: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RankedRepoFile {
    pub path: String,
    pub score: f64,
    pub signatures: Vec<RepoSignature>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MapLang {
    Rust,
    TypeScript,
    Tsx,
    JavaScript,
    Python,
    Go,
    Fallback,
}

struct FileAnalysis {
    norm_path: String,
    display_path: String,
    lang: MapLang,
    signatures: Vec<RepoSignature>,
    import_statements: Vec<String>,
    identifiers: HashMap<String, usize>,
}

/// Builds a token-budgeted repository map ordered by 12-iteration Personalized PageRank.
pub fn build_pagerank_repo_map(
    files: &[RepoFileInput],
    seed: &RepoMapSeed,
    token_budget_chars: usize,
) -> String {
    if files.is_empty() || token_budget_chars == 0 {
        return String::new();
    }

    let ranked = rank_repo_files(files, seed);
    format_ranked_repo_map(&ranked, token_budget_chars)
}

/// Computes Personalized PageRank scores and extracted top-level signatures for all input files.
pub fn rank_repo_files(files: &[RepoFileInput], seed: &RepoMapSeed) -> Vec<RankedRepoFile> {
    let n = files.len();
    if n == 0 {
        return Vec::new();
    }

    let analyses: Vec<FileAnalysis> = files.iter().map(analyze_file).collect();
    let edges = build_dependency_graph(&analyses);
    let teleport = build_teleport_distribution(&analyses, seed);
    let scores = run_personalized_pagerank(n, &edges, &teleport);

    let mut ranked: Vec<RankedRepoFile> = analyses
        .into_iter()
        .enumerate()
        .map(|(idx, analysis)| RankedRepoFile {
            path: analysis.display_path,
            score: scores[idx],
            signatures: select_key_signatures(analysis.signatures),
        })
        .collect();

    ranked.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.path.cmp(&b.path))
    });

    ranked
}

fn detect_map_lang(path: &str) -> MapLang {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());

    match ext.as_deref() {
        Some("rs") => MapLang::Rust,
        Some("ts") | Some("mts") | Some("cts") => MapLang::TypeScript,
        Some("tsx") => MapLang::Tsx,
        Some("js") | Some("jsx") | Some("mjs") | Some("cjs") => MapLang::JavaScript,
        Some("py") | Some("pyi") => MapLang::Python,
        Some("go") => MapLang::Go,
        _ => MapLang::Fallback,
    }
}

fn ts_language_for(lang: MapLang) -> Option<Language> {
    match lang {
        MapLang::Rust => Some(tree_sitter_rust::LANGUAGE.into()),
        MapLang::TypeScript => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        MapLang::Tsx => Some(tree_sitter_typescript::LANGUAGE_TSX.into()),
        MapLang::JavaScript => Some(tree_sitter_javascript::LANGUAGE.into()),
        MapLang::Python => Some(tree_sitter_python::LANGUAGE.into()),
        MapLang::Go => Some(tree_sitter_go::LANGUAGE.into()),
        MapLang::Fallback => None,
    }
}

fn normalize_rel_path(path: &str) -> String {
    let slash = path.trim().replace('\\', "/");
    let trimmed = slash.trim_start_matches("./").trim_matches('/');
    collapse_dot_segments(trimmed)
}

fn collapse_dot_segments(path: &str) -> String {
    let mut stack: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if let Some(last) = stack.last() {
                    if *last != ".." {
                        stack.pop();
                        continue;
                    }
                }
                stack.push("..");
            }
            other => stack.push(other),
        }
    }
    stack.join("/")
}

fn analyze_file(file: &RepoFileInput) -> FileAnalysis {
    let norm_path = normalize_rel_path(&file.path);
    let display_path = if norm_path.is_empty() {
        file.path.trim().to_string()
    } else {
        norm_path.clone()
    };
    let lang = detect_map_lang(&display_path);

    let mut signatures = Vec::new();
    let mut import_statements = Vec::new();

    if !file.content.trim().is_empty() {
        if let Some(ts_lang) = ts_language_for(lang) {
            let mut parser = Parser::new();
            if parser.set_language(&ts_lang).is_ok() {
                if let Some(tree) = parser.parse(&file.content, None) {
                    let root = tree.root_node();
                    collect_ts_imports(root, &file.content, lang, &mut import_statements);
                    collect_ts_signatures(root, &file.content, lang, false, None, &mut signatures);
                }
            }
        } else {
            collect_fallback_signatures(&file.content, &mut signatures);
        }
    }

    let identifiers = extract_identifier_counts(&file.content);

    FileAnalysis {
        norm_path,
        display_path,
        lang,
        signatures,
        import_statements,
        identifiers,
    }
}

fn node_slice<'a>(node: Node, source: &'a str) -> &'a str {
    source.get(node.byte_range()).unwrap_or("")
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn extract_clean_signature(
    sig_start_byte: usize,
    decl_node: Node,
    source: &str,
    body_fields: &[&str],
) -> String {
    let mut end_byte = decl_node.end_byte();
    for &field in body_fields {
        if let Some(body) = decl_node.child_by_field_name(field) {
            end_byte = end_byte.min(body.start_byte());
        }
    }
    let raw = source
        .get(sig_start_byte..end_byte)
        .unwrap_or_else(|| node_slice(decl_node, source));
    let slice = if body_fields.is_empty() {
        raw.lines().next().unwrap_or(raw)
    } else {
        raw
    };
    let collapsed = collapse_ws(slice);
    let without_arrow = collapsed.trim().trim_end_matches("=>").trim();
    let trimmed = without_arrow
        .trim_end_matches(['{', ':', ';', '=', ' '])
        .trim();
    if trimmed.chars().count() <= MAX_SIGNATURE_CHARS {
        trimmed.to_string()
    } else {
        let mut s: String = trimmed.chars().take(MAX_SIGNATURE_CHARS).collect();
        s.push_str("...");
        s
    }
}

fn collect_ts_imports(node: Node, source: &str, lang: MapLang, out: &mut Vec<String>) {
    match lang {
        MapLang::Rust => {
            if node.kind() == "use_declaration" {
                let text = collapse_ws(node_slice(node, source));
                if !text.is_empty() {
                    out.push(text);
                }
                return;
            }
            if node.kind() == "mod_item" && node.child_by_field_name("body").is_none() {
                let text = collapse_ws(node_slice(node, source));
                if !text.is_empty() {
                    out.push(text);
                }
                return;
            }
        }
        MapLang::TypeScript | MapLang::Tsx | MapLang::JavaScript => {
            if node.kind() == "import_statement" || node.kind() == "export_statement" {
                if let Some(src_node) = node.child_by_field_name("source") {
                    let spec = unquote_literal(node_slice(src_node, source));
                    if !spec.is_empty() {
                        out.push(spec);
                    }
                }
            } else if node.kind() == "call_expression" {
                if let Some(func) = node.child_by_field_name("function") {
                    let fn_name = node_slice(func, source).trim();
                    if fn_name == "require" || fn_name == "import" {
                        if let Some(args) = node.child_by_field_name("arguments") {
                            let mut cursor = args.walk();
                            for child in args.children(&mut cursor) {
                                if child.kind() == "string" {
                                    let spec = unquote_literal(node_slice(child, source));
                                    if !spec.is_empty() {
                                        out.push(spec);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        MapLang::Python => {
            if node.kind() == "import_statement" || node.kind() == "import_from_statement" {
                let text = collapse_ws(node_slice(node, source));
                if !text.is_empty() {
                    out.push(text);
                }
                return;
            }
        }
        MapLang::Go => {
            if node.kind() == "import_spec" {
                if let Some(path_node) = node.child_by_field_name("path") {
                    let spec = unquote_literal(node_slice(path_node, source));
                    if !spec.is_empty() {
                        out.push(spec);
                    }
                }
                return;
            }
        }
        MapLang::Fallback => return,
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_ts_imports(child, source, lang, out);
    }
}

fn unquote_literal(raw: &str) -> String {
    raw.trim()
        .trim_matches(['"', '\'', '`'])
        .trim()
        .to_string()
}

fn collect_ts_signatures(
    node: Node,
    source: &str,
    lang: MapLang,
    is_member: bool,
    export_prefix_start: Option<usize>,
    out: &mut Vec<RepoSignature>,
) {
    if out.len() >= 64 {
        return;
    }

    match lang {
        MapLang::Rust => collect_rust_sigs(node, source, is_member, out),
        MapLang::TypeScript | MapLang::Tsx | MapLang::JavaScript => {
            collect_jsts_sigs(node, source, lang, is_member, export_prefix_start, out)
        }
        MapLang::Python => collect_python_sigs(node, source, is_member, out),
        MapLang::Go => collect_go_sigs(node, source, out),
        MapLang::Fallback => {}
    }
}

fn collect_rust_sigs(
    node: Node,
    source: &str,
    is_member: bool,
    out: &mut Vec<RepoSignature>,
) {
    match node.kind() {
        "function_item" | "function_signature_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_slice(name_node, source).trim().to_string();
                if !name.is_empty() {
                    let sig = extract_clean_signature(node.start_byte(), node, source, &["body"]);
                    out.push(RepoSignature {
                        name,
                        kind: if is_member { "method" } else { "fn" }.to_string(),
                        line: node.start_position().row + 1,
                        signature: sig,
                        is_member,
                    });
                }
            }
            return;
        }
        "struct_item" | "enum_item" | "union_item" | "type_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_slice(name_node, source).trim().to_string();
                if !name.is_empty() {
                    let kind = match node.kind() {
                        "struct_item" => "struct",
                        "enum_item" => "enum",
                        "union_item" => "union",
                        _ => "type",
                    };
                    let body_fields: &[&str] = if node.kind() == "type_item" {
                        &[]
                    } else {
                        &["body"]
                    };
                    let sig =
                        extract_clean_signature(node.start_byte(), node, source, body_fields);
                    out.push(RepoSignature {
                        name,
                        kind: kind.to_string(),
                        line: node.start_position().row + 1,
                        signature: sig,
                        is_member,
                    });
                }
            }
            return;
        }
        "trait_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_slice(name_node, source).trim().to_string();
                if !name.is_empty() {
                    let sig = extract_clean_signature(node.start_byte(), node, source, &["body"]);
                    out.push(RepoSignature {
                        name,
                        kind: "trait".to_string(),
                        line: node.start_position().row + 1,
                        signature: sig,
                        is_member: false,
                    });
                }
            }
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.children(&mut cursor) {
                    collect_rust_sigs(child, source, true, out);
                }
            }
            return;
        }
        "impl_item" => {
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.children(&mut cursor) {
                    collect_rust_sigs(child, source, true, out);
                }
            }
            return;
        }
        "mod_item" => {
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.children(&mut cursor) {
                    collect_rust_sigs(child, source, false, out);
                }
            }
            return;
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_rust_sigs(child, source, is_member, out);
    }
}

fn collect_jsts_sigs(
    node: Node,
    source: &str,
    lang: MapLang,
    is_member: bool,
    export_start: Option<usize>,
    out: &mut Vec<RepoSignature>,
) {
    let sig_start = export_start.unwrap_or_else(|| node.start_byte());
    let line = node.start_position().row + 1;

    match node.kind() {
        "export_statement" => {
            let start = node.start_byte();
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_jsts_sigs(child, source, lang, is_member, Some(start), out);
            }
            return;
        }
        "function_declaration" | "generator_function_declaration" | "function_signature" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_slice(name_node, source).trim().to_string();
                if !name.is_empty() {
                    let sig = extract_clean_signature(sig_start, node, source, &["body"]);
                    out.push(RepoSignature {
                        name,
                        kind: "function".to_string(),
                        line,
                        signature: sig,
                        is_member,
                    });
                }
            }
            return;
        }
        "class_declaration" | "abstract_class_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_slice(name_node, source).trim().to_string();
                if !name.is_empty() {
                    let sig = extract_clean_signature(sig_start, node, source, &["body"]);
                    out.push(RepoSignature {
                        name,
                        kind: "class".to_string(),
                        line,
                        signature: sig,
                        is_member: false,
                    });
                }
            }
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.children(&mut cursor) {
                    collect_jsts_sigs(child, source, lang, true, None, out);
                }
            }
            return;
        }
        "interface_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_slice(name_node, source).trim().to_string();
                if !name.is_empty() {
                    let sig = extract_clean_signature(sig_start, node, source, &["body"]);
                    out.push(RepoSignature {
                        name,
                        kind: "interface".to_string(),
                        line,
                        signature: sig,
                        is_member: false,
                    });
                }
            }
            return;
        }
        "type_alias_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_slice(name_node, source).trim().to_string();
                if !name.is_empty() {
                    let sig = extract_clean_signature(sig_start, node, source, &[]);
                    out.push(RepoSignature {
                        name,
                        kind: "type".to_string(),
                        line,
                        signature: sig,
                        is_member: false,
                    });
                }
            }
            return;
        }
        "enum_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_slice(name_node, source).trim().to_string();
                if !name.is_empty() {
                    let sig = extract_clean_signature(sig_start, node, source, &["body"]);
                    out.push(RepoSignature {
                        name,
                        kind: "enum".to_string(),
                        line,
                        signature: sig,
                        is_member: false,
                    });
                }
            }
            return;
        }
        "method_definition" | "method_signature" | "abstract_method_signature" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_slice(name_node, source).trim().to_string();
                if !name.is_empty() && name != "constructor" {
                    let sig = extract_clean_signature(node.start_byte(), node, source, &["body"]);
                    out.push(RepoSignature {
                        name,
                        kind: "method".to_string(),
                        line,
                        signature: sig,
                        is_member: true,
                    });
                }
            }
            return;
        }
        "lexical_declaration" | "variable_declaration" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "variable_declarator" {
                    if let (Some(name_node), Some(val_node)) = (
                        child.child_by_field_name("name"),
                        child.child_by_field_name("value"),
                    ) {
                        if matches!(
                            val_node.kind(),
                            "arrow_function" | "function_expression" | "generator_function"
                        ) {
                            let name = node_slice(name_node, source).trim().to_string();
                            if !name.is_empty() {
                                let sig =
                                    extract_clean_signature(sig_start, val_node, source, &["body"]);
                                out.push(RepoSignature {
                                    name,
                                    kind: "function".to_string(),
                                    line,
                                    signature: sig,
                                    is_member,
                                });
                            }
                        }
                    }
                }
            }
            return;
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_jsts_sigs(child, source, lang, is_member, None, out);
    }
}

fn collect_python_sigs(
    node: Node,
    source: &str,
    is_member: bool,
    out: &mut Vec<RepoSignature>,
) {
    match node.kind() {
        "decorated_definition" => {
            if let Some(def) = node.child_by_field_name("definition") {
                collect_python_sigs(def, source, is_member, out);
            }
            return;
        }
        "function_definition" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_slice(name_node, source).trim().to_string();
                if !name.is_empty() {
                    let sig = extract_clean_signature(node.start_byte(), node, source, &["body"]);
                    out.push(RepoSignature {
                        name,
                        kind: if is_member { "method" } else { "function" }.to_string(),
                        line: node.start_position().row + 1,
                        signature: sig,
                        is_member,
                    });
                }
            }
            return;
        }
        "class_definition" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_slice(name_node, source).trim().to_string();
                if !name.is_empty() {
                    let sig = extract_clean_signature(node.start_byte(), node, source, &["body"]);
                    out.push(RepoSignature {
                        name,
                        kind: "class".to_string(),
                        line: node.start_position().row + 1,
                        signature: sig,
                        is_member: false,
                    });
                }
            }
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.children(&mut cursor) {
                    collect_python_sigs(child, source, true, out);
                }
            }
            return;
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_python_sigs(child, source, is_member, out);
    }
}

fn collect_go_sigs(node: Node, source: &str, out: &mut Vec<RepoSignature>) {
    match node.kind() {
        "function_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_slice(name_node, source).trim().to_string();
                if !name.is_empty() {
                    let sig = extract_clean_signature(node.start_byte(), node, source, &["body"]);
                    out.push(RepoSignature {
                        name,
                        kind: "function".to_string(),
                        line: node.start_position().row + 1,
                        signature: sig,
                        is_member: false,
                    });
                }
            }
            return;
        }
        "method_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_slice(name_node, source).trim().to_string();
                if !name.is_empty() {
                    let sig = extract_clean_signature(node.start_byte(), node, source, &["body"]);
                    out.push(RepoSignature {
                        name,
                        kind: "method".to_string(),
                        line: node.start_position().row + 1,
                        signature: sig,
                        is_member: true,
                    });
                }
            }
            return;
        }
        "type_declaration" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "type_spec" {
                    if let Some(name_node) = child.child_by_field_name("name") {
                        let name = node_slice(name_node, source).trim().to_string();
                        if !name.is_empty() {
                            let kind = match child.child_by_field_name("type").map(|t| t.kind()) {
                                Some("struct_type") => "struct",
                                Some("interface_type") => "interface",
                                _ => "type",
                            };
                            let sig = extract_clean_signature(
                                child.start_byte(),
                                child,
                                source,
                                &[],
                            );
                            out.push(RepoSignature {
                                name,
                                kind: kind.to_string(),
                                line: child.start_position().row + 1,
                                signature: format!("type {sig}"),
                                is_member: false,
                            });
                        }
                    }
                }
            }
            return;
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_go_sigs(child, source, out);
    }
}

fn collect_fallback_signatures(source: &str, out: &mut Vec<RepoSignature>) {
    for (idx, raw_line) in source.lines().enumerate() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with('#') {
            continue;
        }
        let clean = trimmed
            .strip_prefix("pub(crate) ")
            .or_else(|| trimmed.strip_prefix("pub "))
            .or_else(|| trimmed.strip_prefix("export default "))
            .or_else(|| trimmed.strip_prefix("export "))
            .unwrap_or(trimmed);
        let clean = clean.strip_prefix("async ").unwrap_or(clean);

        let matched = if let Some(rest) = clean.strip_prefix("fn ") {
            Some(("fn", rest))
        } else if let Some(rest) = clean.strip_prefix("function ") {
            Some(("function", rest))
        } else if let Some(rest) = clean.strip_prefix("def ") {
            Some(("function", rest))
        } else if let Some(rest) = clean.strip_prefix("struct ") {
            Some(("struct", rest))
        } else if let Some(rest) = clean.strip_prefix("trait ") {
            Some(("trait", rest))
        } else if let Some(rest) = clean.strip_prefix("enum ") {
            Some(("enum", rest))
        } else if let Some(rest) = clean.strip_prefix("class ") {
            Some(("class", rest))
        } else if let Some(rest) = clean.strip_prefix("interface ") {
            Some(("interface", rest))
        } else if let Some(rest) = clean.strip_prefix("type ") {
            Some(("type", rest))
        } else {
            None
        };

        if let Some((kind, rest)) = matched {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if name.len() >= 2 {
                let sig: String = trimmed
                    .trim_end_matches(['{', ':', ';', ' '])
                    .chars()
                    .take(MAX_SIGNATURE_CHARS)
                    .collect();
                out.push(RepoSignature {
                    name,
                    kind: kind.to_string(),
                    line: idx + 1,
                    signature: sig,
                    is_member: false,
                });
                if out.len() >= 32 {
                    break;
                }
            }
        }
    }
}

fn select_key_signatures(mut sigs: Vec<RepoSignature>) -> Vec<RepoSignature> {
    if sigs.len() <= MAX_SIGNATURES_PER_FILE {
        sigs.sort_by_key(|s| s.line);
        return sigs;
    }

    // Prioritize top-level declarations before member methods when capping per-file signatures.
    sigs.sort_by(|a, b| a.is_member.cmp(&b.is_member).then_with(|| a.line.cmp(&b.line)));
    sigs.truncate(MAX_SIGNATURES_PER_FILE);
    sigs.sort_by_key(|s| s.line);
    sigs
}

fn extract_identifier_counts(source: &str) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for tok in source.split(|c: char| !c.is_alphanumeric() && c != '_') {
        if tok.len() < 3 {
            continue;
        }
        let first = tok.chars().next().unwrap_or('0');
        if first.is_ascii_digit() {
            continue;
        }
        if is_noise_keyword(tok) {
            continue;
        }
        let entry = counts.entry(tok.to_string()).or_insert(0usize);
        if *entry < 20 {
            *entry += 1;
        }
    }
    counts
}

fn is_noise_keyword(tok: &str) -> bool {
    matches!(
        tok,
        "pub"
            | "use"
            | "mod"
            | "let"
            | "mut"
            | "const"
            | "static"
            | "fn"
            | "struct"
            | "enum"
            | "trait"
            | "impl"
            | "type"
            | "where"
            | "for"
            | "while"
            | "loop"
            | "match"
            | "return"
            | "else"
            | "async"
            | "await"
            | "self"
            | "Self"
            | "super"
            | "crate"
            | "true"
            | "false"
            | "Some"
            | "None"
            | "Ok"
            | "Err"
            | "Result"
            | "Option"
            | "String"
            | "Vec"
            | "usize"
            | "isize"
            | "bool"
            | "u64"
            | "i64"
            | "u32"
            | "i32"
            | "f64"
            | "f32"
            | "str"
            | "import"
            | "export"
            | "from"
            | "default"
            | "function"
            | "class"
            | "interface"
            | "extends"
            | "implements"
            | "string"
            | "number"
            | "boolean"
            | "void"
            | "null"
            | "undefined"
            | "this"
            | "def"
            | "pass"
            | "True"
            | "False"
            | "package"
            | "func"
            | "var"
            | "nil"
            | "error"
            | "new"
            | "main"
            | "test"
            | "tests"
            | "init"
            | "clone"
            | "fmt"
    )
}

fn strip_known_ext(path: &str) -> &str {
    for suffix in [
        ".d.ts", ".tsx", ".ts", ".mts", ".cts", ".jsx", ".js", ".mjs", ".cjs", ".rs", ".py",
        ".pyi", ".go", ".json",
    ] {
        if let Some(stem) = path.strip_suffix(suffix) {
            return stem;
        }
    }
    path
}

fn parent_dir_str(norm_path: &str) -> &str {
    match norm_path.rsplit_once('/') {
        Some((dir, _)) => dir,
        None => "",
    }
}

fn build_dependency_graph(analyses: &[FileAnalysis]) -> Vec<HashMap<usize, f64>> {
    let n = analyses.len();
    let mut adj: Vec<HashMap<usize, f64>> = (0..n).map(|_| HashMap::new()).collect();

    let mut exact_path_map: HashMap<String, usize> = HashMap::new();
    let mut stem_map: HashMap<String, Vec<usize>> = HashMap::new();
    let mut dir_map: HashMap<String, Vec<usize>> = HashMap::new();

    for (idx, a) in analyses.iter().enumerate() {
        exact_path_map.insert(a.norm_path.clone(), idx);
        let stem = strip_known_ext(&a.norm_path).to_string();
        stem_map.entry(stem.clone()).or_default().push(idx);

        // Index module entrypoints (`foo/index`, `foo/mod`, `foo/__init__`) under `foo` as well.
        for index_suffix in ["/index", "/mod", "/__init__"] {
            if let Some(dir_stem) = stem.strip_suffix(index_suffix) {
                stem_map.entry(dir_stem.to_string()).or_default().push(idx);
            }
        }

        let dir = parent_dir_str(&a.norm_path).to_string();
        dir_map.entry(dir).or_default().push(idx);
    }

    // 1. Explicit import / use / mod / from edges.
    for (u, a) in analyses.iter().enumerate() {
        let mut import_targets = HashSet::new();
        for stmt in &a.import_statements {
            resolve_import_targets(
                a,
                stmt,
                &exact_path_map,
                &stem_map,
                &dir_map,
                analyses,
                &mut import_targets,
            );
        }

        for v in import_targets {
            if u != v {
                add_edge(&mut adj, u, v, EDGE_IMPORT_FORWARD);
                add_edge(&mut adj, v, u, EDGE_IMPORT_REVERSE);
            }
        }
    }

    // 2. Cross-file symbol definition -> mention edges.
    let mut symbol_definers: HashMap<&str, Vec<usize>> = HashMap::new();
    for (idx, a) in analyses.iter().enumerate() {
        let mut seen_in_file = HashSet::new();
        for sig in &a.signatures {
            if sig.name.len() < 3 || is_noise_keyword(&sig.name) {
                continue;
            }
            // Prefer top-level symbols; include methods only if distinct enough (>= 5 chars).
            if sig.is_member && sig.name.len() < 5 {
                continue;
            }
            if seen_in_file.insert(sig.name.as_str()) {
                symbol_definers
                    .entry(sig.name.as_str())
                    .or_default()
                    .push(idx);
            }
        }
    }

    for (u, a) in analyses.iter().enumerate() {
        for (ident, &mention_count) in &a.identifiers {
            let Some(definers) = symbol_definers.get(ident.as_str()) else {
                continue;
            };
            if definers.is_empty() || definers.len() > MAX_SYMBOL_DEFINERS {
                continue;
            }
            let specificity = 1.0 / (definers.len() as f64);
            let tf_boost = 1.0 + 0.25 * ((mention_count.min(5) - 1) as f64);
            let forward_w = specificity * tf_boost;
            let reverse_w = forward_w * EDGE_SYMBOL_REVERSE_FACTOR;

            for &v in definers {
                if u != v {
                    add_edge(&mut adj, u, v, forward_w);
                    add_edge(&mut adj, v, u, reverse_w);
                }
            }
        }
    }

    adj
}

fn add_edge(adj: &mut [HashMap<usize, f64>], from: usize, to: usize, weight: f64) {
    if weight > 0.0 && from != to {
        *adj[from].entry(to).or_insert(0.0) += weight;
    }
}

fn resolve_import_targets(
    importer: &FileAnalysis,
    stmt: &str,
    exact_path_map: &HashMap<String, usize>,
    stem_map: &HashMap<String, Vec<usize>>,
    dir_map: &HashMap<String, Vec<usize>>,
    all_files: &[FileAnalysis],
    out: &mut HashSet<usize>,
) {
    let importer_dir = parent_dir_str(&importer.norm_path);

    match importer.lang {
        MapLang::TypeScript | MapLang::Tsx | MapLang::JavaScript => {
            let spec = stmt.trim();
            if spec.is_empty() {
                return;
            }
            let clean_spec = strip_known_ext(spec);
            if spec.starts_with("./") || spec.starts_with("../") {
                let joined = if importer_dir.is_empty() {
                    collapse_dot_segments(clean_spec)
                } else {
                    collapse_dot_segments(&format!("{importer_dir}/{clean_spec}"))
                };
                if let Some(&idx) = exact_path_map.get(&collapse_dot_segments(spec)) {
                    out.insert(idx);
                }
                if let Some(indices) = stem_map.get(&joined) {
                    out.extend(indices.iter().copied());
                }
            } else {
                let alias_trimmed = clean_spec
                    .strip_prefix("@/")
                    .or_else(|| clean_spec.strip_prefix("~/"))
                    .unwrap_or(clean_spec);
                match_stem_or_suffix(alias_trimmed, stem_map, out);
            }
        }
        MapLang::Rust => {
            // Handle `mod foo;` / `pub mod foo;`
            let trimmed = stmt.trim().trim_end_matches(';').trim();
            if let Some(mod_rest) = trimmed
                .strip_prefix("pub(crate) mod ")
                .or_else(|| trimmed.strip_prefix("pub mod "))
                .or_else(|| trimmed.strip_prefix("mod "))
            {
                let mod_name = mod_rest.trim();
                let candidate = if importer_dir.is_empty() {
                    mod_name.to_string()
                } else {
                    format!("{importer_dir}/{mod_name}")
                };
                if let Some(indices) = stem_map.get(&candidate) {
                    out.extend(indices.iter().copied());
                }
                return;
            }

            // Handle `use crate::foo::bar;` / `use super::foo;` / `use crate::foo::{a, b};`
            if let Some(use_rest) = trimmed
                .strip_prefix("pub(crate) use ")
                .or_else(|| trimmed.strip_prefix("pub use "))
                .or_else(|| trimmed.strip_prefix("use "))
            {
                let base_path = use_rest.split('{').next().unwrap_or(use_rest).trim();
                let segs: Vec<&str> = base_path
                    .split("::")
                    .map(str::trim)
                    .filter(|s| !s.is_empty() && *s != "*")
                    .collect();
                if segs.is_empty() {
                    return;
                }

                let crate_root = find_rust_src_root(&importer.norm_path);
                match segs[0] {
                    "crate" => {
                        collect_rust_prefix_matches(&crate_root, &segs[1..], stem_map, out);
                    }
                    "self" => {
                        collect_rust_prefix_matches(importer_dir, &segs[1..], stem_map, out);
                    }
                    "super" => {
                        let is_mod_entry = importer.norm_path.ends_with("/mod.rs")
                            || importer.norm_path.ends_with("/lib.rs")
                            || importer.norm_path.ends_with("/main.rs");
                        let super_dir = if is_mod_entry {
                            parent_dir_str(importer_dir)
                        } else {
                            importer_dir
                        };
                        collect_rust_prefix_matches(super_dir, &segs[1..], stem_map, out);
                    }
                    _ => {
                        collect_rust_prefix_matches(&crate_root, &segs, stem_map, out);
                        collect_rust_prefix_matches(importer_dir, &segs, stem_map, out);
                    }
                }
            }
        }
        MapLang::Python => {
            // `from ..pkg.mod import sym` or `import pkg.mod`
            let trimmed = stmt.trim();
            if let Some(rest) = trimmed.strip_prefix("from ") {
                let mod_part = rest.split_whitespace().next().unwrap_or("");
                resolve_python_module(importer_dir, mod_part, stem_map, out);
                if let Some((_, after_import)) = rest.split_once(" import ") {
                    for item in after_import.split(',') {
                        let sub = item
                            .split_whitespace()
                            .next()
                            .unwrap_or("")
                            .trim_matches(['(', ')']);
                        if !sub.is_empty() && sub != "*" {
                            let combined = if mod_part.ends_with('.') {
                                format!("{mod_part}{sub}")
                            } else {
                                format!("{mod_part}.{sub}")
                            };
                            resolve_python_module(importer_dir, &combined, stem_map, out);
                        }
                    }
                }
            } else if let Some(rest) = trimmed.strip_prefix("import ") {
                for part in rest.split(',') {
                    let mod_part = part.split_whitespace().next().unwrap_or("");
                    resolve_python_module(importer_dir, mod_part, stem_map, out);
                }
            }
        }
        MapLang::Go => {
            let spec = stmt.trim();
            if spec.is_empty() {
                return;
            }
            for (dir, indices) in dir_map {
                if !dir.is_empty() && (spec == dir || spec.ends_with(&format!("/{dir}"))) {
                    for &idx in indices {
                        if all_files[idx].lang == MapLang::Go {
                            out.insert(idx);
                        }
                    }
                }
            }
        }
        MapLang::Fallback => {}
    }
}

fn find_rust_src_root(norm_path: &str) -> String {
    if norm_path.starts_with("src/") {
        return "src".to_string();
    }
    if let Some((prefix, _)) = norm_path.rsplit_once("/src/") {
        return format!("{prefix}/src");
    }
    parent_dir_str(norm_path).to_string()
}

fn collect_rust_prefix_matches(
    base_dir: &str,
    segs: &[&str],
    stem_map: &HashMap<String, Vec<usize>>,
    out: &mut HashSet<usize>,
) {
    let mut current = base_dir.to_string();
    for &seg in segs {
        current = if current.is_empty() {
            seg.to_string()
        } else {
            format!("{current}/{seg}")
        };
        if let Some(indices) = stem_map.get(&current) {
            out.extend(indices.iter().copied());
        }
    }
}

fn resolve_python_module(
    importer_dir: &str,
    mod_spec: &str,
    stem_map: &HashMap<String, Vec<usize>>,
    out: &mut HashSet<usize>,
) {
    if mod_spec.is_empty() {
        return;
    }
    let leading_dots = mod_spec.chars().take_while(|&c| c == '.').count();
    let remainder = mod_spec[leading_dots..].replace('.', "/");

    if leading_dots > 0 {
        let mut base = importer_dir.to_string();
        for _ in 1..leading_dots {
            base = parent_dir_str(&base).to_string();
        }
        let candidate = if remainder.is_empty() {
            base
        } else if base.is_empty() {
            remainder
        } else {
            format!("{base}/{remainder}")
        };
        if let Some(indices) = stem_map.get(&candidate) {
            out.extend(indices.iter().copied());
        }
    } else if !remainder.is_empty() {
        match_stem_or_suffix(&remainder, stem_map, out);
    }
}

fn match_stem_or_suffix(
    target_stem: &str,
    stem_map: &HashMap<String, Vec<usize>>,
    out: &mut HashSet<usize>,
) {
    if let Some(indices) = stem_map.get(target_stem) {
        out.extend(indices.iter().copied());
        return;
    }
    let suffix = format!("/{target_stem}");
    for (stem, indices) in stem_map {
        if stem.ends_with(&suffix) {
            out.extend(indices.iter().copied());
        }
    }
}

fn path_matches_seed_list(norm_file_path: &str, seed_list: &[String]) -> bool {
    for raw in seed_list {
        let norm_seed = normalize_rel_path(raw);
        if norm_seed.is_empty() {
            continue;
        }
        if norm_seed == norm_file_path
            || norm_seed.ends_with(&format!("/{norm_file_path}"))
            || norm_file_path.ends_with(&format!("/{norm_seed}"))
        {
            return true;
        }
    }
    false
}

fn normalize_query_terms(raw_terms: &[String]) -> Vec<String> {
    let mut terms = Vec::new();
    let mut seen = HashSet::new();
    for raw in raw_terms {
        for tok in raw.split(|c: char| !c.is_alphanumeric() && c != '_') {
            if tok.len() >= 2 {
                let lower = tok.to_ascii_lowercase();
                if seen.insert(lower.clone()) {
                    terms.push(lower);
                }
            }
        }
    }
    terms
}

fn build_teleport_distribution(analyses: &[FileAnalysis], seed: &RepoMapSeed) -> Vec<f64> {
    let query_terms = normalize_query_terms(&seed.query_terms);
    let mut weights = Vec::with_capacity(analyses.len());

    for a in analyses {
        let mut w = TELEPORT_BASELINE;

        if path_matches_seed_list(&a.norm_path, &seed.active_files) {
            w += TELEPORT_ACTIVE_FILE;
        }
        if path_matches_seed_list(&a.norm_path, &seed.dirty_files) {
            w += TELEPORT_DIRTY_FILE;
        }

        if !query_terms.is_empty() {
            let path_lower = a.norm_path.to_ascii_lowercase();
            let mut strong_matches = 0usize;
            let mut body_matches = 0usize;

            for term in &query_terms {
                let in_path = path_lower.contains(term.as_str());
                let in_sig = a
                    .signatures
                    .iter()
                    .any(|s| s.name.to_ascii_lowercase().contains(term.as_str()));
                if in_path || in_sig {
                    strong_matches += 1;
                } else if a
                    .identifiers
                    .keys()
                    .any(|id| id.to_ascii_lowercase() == *term || (term.len() >= 4 && id.to_ascii_lowercase().contains(term.as_str())))
                {
                    body_matches += 1;
                }
            }

            if strong_matches > 0 {
                w += TELEPORT_QUERY_MATCH * (1.0 + 0.30 * ((strong_matches - 1) as f64));
            } else if body_matches > 0 {
                w += TELEPORT_QUERY_MATCH * 0.65 * (1.0 + 0.20 * ((body_matches - 1) as f64));
            }
        }

        weights.push(w);
    }

    let sum: f64 = weights.iter().sum();
    if sum > 0.0 {
        for w in &mut weights {
            *w /= sum;
        }
    } else {
        let uniform = 1.0 / (analyses.len() as f64);
        weights.fill(uniform);
    }

    weights
}

fn run_personalized_pagerank(
    n: usize,
    adj: &[HashMap<usize, f64>],
    teleport: &[f64],
) -> Vec<f64> {
    if n == 0 {
        return Vec::new();
    }

    let out_totals: Vec<f64> = adj.iter().map(|m| m.values().sum()).collect();
    let mut rank = teleport.to_vec();
    let mut next = vec![0.0f64; n];

    for _ in 0..PPR_ITERATIONS {
        let mut dangling_mass = 0.0f64;
        for u in 0..n {
            if out_totals[u] <= 1e-12 {
                dangling_mass += rank[u];
            }
        }

        let base_factor = (1.0 - PPR_DAMPING) + PPR_DAMPING * dangling_mass;
        for v in 0..n {
            next[v] = base_factor * teleport[v];
        }

        for u in 0..n {
            let total = out_totals[u];
            if total > 1e-12 {
                let scale = PPR_DAMPING * (rank[u] / total);
                for (&v, &w) in &adj[u] {
                    next[v] += scale * w;
                }
            }
        }

        std::mem::swap(&mut rank, &mut next);
    }

    rank
}

fn format_ranked_repo_map(ranked: &[RankedRepoFile], token_budget_chars: usize) -> String {
    let mut out = String::new();

    for file in ranked {
        let file_header = format!("{}:\n", file.path);

        if file.signatures.is_empty() {
            let compact = format!("{}\n", file.path);
            if out.len() + compact.len() <= token_budget_chars {
                out.push_str(&compact);
            } else if out.is_empty() {
                return truncate_exact(&compact, token_budget_chars);
            } else {
                break;
            }
            continue;
        }

        let first_sig_line = format!(
            "  L{}: {}\n",
            file.signatures[0].line, file.signatures[0].signature
        );

        if out.len() + file_header.len() + first_sig_line.len() > token_budget_chars {
            if out.is_empty() {
                if file_header.len() <= token_budget_chars {
                    out.push_str(&file_header);
                } else {
                    return truncate_exact(&file_header, token_budget_chars);
                }
            }
            break;
        }

        out.push_str(&file_header);
        for sig in &file.signatures {
            let line_str = format!("  L{}: {}\n", sig.line, sig.signature);
            if out.len() + line_str.len() <= token_budget_chars {
                out.push_str(&line_str);
            } else {
                return out;
            }
        }
    }

    out
}

fn truncate_exact(s: &str, max_bytes: usize) -> String {
    let mut acc = String::new();
    for ch in s.chars() {
        if acc.len() + ch.len_utf8() > max_bytes {
            break;
        }
        acc.push(ch);
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pagerank_ranks_imported_core_types_above_unrelated_files() {
        let files = vec![
            RepoFileInput {
                path: "src/core/types.rs".into(),
                content: r#"
pub struct SessionClaims {
    pub user_id: String,
    pub role: String,
}

pub trait TokenValidator {
    fn validate(&self, raw: &str) -> Option<SessionClaims>;
}
"#
                .into(),
            },
            RepoFileInput {
                path: "src/auth/jwt.rs".into(),
                content: r#"
use crate::core::types::{SessionClaims, TokenValidator};

pub struct JwtValidator;

impl TokenValidator for JwtValidator {
    fn validate(&self, raw: &str) -> Option<SessionClaims> {
        None
    }
}
"#
                .into(),
            },
            RepoFileInput {
                path: "src/api/middleware.rs".into(),
                content: r#"
use crate::core::types::{SessionClaims, TokenValidator};
use crate::auth::jwt::JwtValidator;

pub fn check_request_auth(header: &str) -> Option<SessionClaims> {
    let validator = JwtValidator;
    validator.validate(header)
}
"#
                .into(),
            },
            RepoFileInput {
                path: "src/billing/invoices.rs".into(),
                content: r#"
pub struct InvoiceSummary {
    pub cents: u64,
}

pub fn format_invoice(cents: u64) -> String {
    format!("${:.2}", (cents as f64) / 100.0)
}
"#
                .into(),
            },
            RepoFileInput {
                path: "src/telemetry/metrics.rs".into(),
                content: r#"
pub fn record_latency_ms(ms: u64) {}
"#
                .into(),
            },
        ];

        // Even with an empty seed, globally imported foundational types (`src/core/types.rs`)
        // should rank at the very top above isolated unrelated modules.
        let ranked = rank_repo_files(&files, &RepoMapSeed::default());
        assert_eq!(ranked[0].path, "src/core/types.rs");

        let core_score = ranked
            .iter()
            .find(|r| r.path == "src/core/types.rs")
            .unwrap()
            .score;
        let billing_score = ranked
            .iter()
            .find(|r| r.path == "src/billing/invoices.rs")
            .unwrap()
            .score;
        let metrics_score = ranked
            .iter()
            .find(|r| r.path == "src/telemetry/metrics.rs")
            .unwrap()
            .score;

        assert!(
            core_score > billing_score && core_score > metrics_score,
            "expected core_score ({core_score}) > billing ({billing_score}) and metrics ({metrics_score})"
        );
    }

    #[test]
    fn pagerank_ranks_seeded_neighbors_and_query_matches_above_unrelated_files() {
        let files = vec![
            RepoFileInput {
                path: "src/domain/order.ts".into(),
                content: r#"
export interface OrderPayload {
    id: string;
    totalCents: number;
}
"#
                .into(),
            },
            RepoFileInput {
                path: "src/services/checkout.ts".into(),
                content: r#"
import { OrderPayload } from "../domain/order";

export class CheckoutService {
    submitOrder(order: OrderPayload): boolean {
        return order.totalCents > 0;
    }
}
"#
                .into(),
            },
            RepoFileInput {
                path: "src/routes/checkoutRoute.ts".into(),
                content: r#"
import { CheckoutService } from "../services/checkout";

export function postCheckout(body: unknown): boolean {
    const svc = new CheckoutService();
    return Boolean(svc);
}
"#
                .into(),
            },
            RepoFileInput {
                path: "src/unrelated/themeColors.ts".into(),
                content: r##"
export const PRIMARY_HEX = "#3366ff";
export function invertHex(hex: string): string {
    return hex;
}
"##
                .into(),
            },
            RepoFileInput {
                path: "src/unrelated/mathHelpers.ts".into(),
                content: r#"
export function clampValue(v: number, min: number, max: number): number {
    return Math.max(min, Math.min(max, v));
}
"#
                .into(),
            },
        ];

        // Seed `checkout.ts` as active; both its imported dependency (`order.ts`)
        // and its direct caller (`checkoutRoute.ts`) should rank above unrelated files.
        let seed = RepoMapSeed {
            active_files: vec!["src/services/checkout.ts".into()],
            dirty_files: vec![],
            query_terms: vec![],
        };

        let ranked = rank_repo_files(&files, &seed);
        let pos = |p: &str| ranked.iter().position(|r| r.path == p).unwrap();

        let active_pos = pos("src/services/checkout.ts");
        let dep_pos = pos("src/domain/order.ts");
        let caller_pos = pos("src/routes/checkoutRoute.ts");
        let theme_pos = pos("src/unrelated/themeColors.ts");
        let math_pos = pos("src/unrelated/mathHelpers.ts");

        assert!(active_pos < theme_pos && active_pos < math_pos);
        assert!(dep_pos < theme_pos && dep_pos < math_pos);
        assert!(caller_pos < theme_pos && caller_pos < math_pos);

        // Also verify query_terms + dirty_files boost relevant targets.
        let query_seed = RepoMapSeed {
            active_files: vec![],
            dirty_files: vec!["src/unrelated/mathHelpers.ts".into()],
            query_terms: vec!["clampValue".into()],
        };
        let query_ranked = rank_repo_files(&files, &query_seed);
        assert_eq!(query_ranked[0].path, "src/unrelated/mathHelpers.ts");
    }

    #[test]
    fn enforces_token_budget_and_formats_line_numbered_signatures_across_languages() {
        let files = vec![
            RepoFileInput {
                path: "app/models.py".into(),
                content: r#"
class UserAccount:
    def is_verified(self) -> bool:
        return True

def load_account(account_id: str) -> UserAccount:
    return UserAccount()
"#
                .into(),
            },
            RepoFileInput {
                path: "app/service.py".into(),
                content: r#"
from .models import UserAccount, load_account

def authenticate_user(account_id: str) -> bool:
    acct = load_account(account_id)
    return acct.is_verified()
"#
                .into(),
            },
            RepoFileInput {
                path: "pkg/store/store.go".into(),
                content: r#"
package store

type RecordStore interface {
    Get(key string) (string, error)
}

func OpenStore(dsn string) RecordStore {
    return nil
}
"#
                .into(),
            },
        ];

        let seed = RepoMapSeed {
            active_files: vec!["app/service.py".into()],
            dirty_files: vec!["pkg/store/store.go".into()],
            query_terms: vec!["UserAccount".into()],
        };

        let full_map = build_pagerank_repo_map(&files, &seed, 4000);
        assert!(full_map.contains("app/models.py:"));
        assert!(full_map.contains("class UserAccount"));
        assert!(full_map.contains("def load_account(account_id: str) -> UserAccount"));
        assert!(full_map.contains("pkg/store/store.go:"));
        assert!(full_map.contains("type RecordStore interface"));
        assert!(full_map.contains("L"));

        // Tight token budget must strictly bound output size.
        let tight_budget = 110;
        let budgeted_map = build_pagerank_repo_map(&files, &seed, tight_budget);
        assert!(!budgeted_map.is_empty());
        assert!(
            budgeted_map.len() <= tight_budget,
            "map len {} exceeded budget {}",
            budgeted_map.len(),
            tight_budget
        );
    }

    #[test]
    fn handles_go_package_imports_symbol_links_and_conserves_probability() {
        let files = vec![
            RepoFileInput {
                path: "internal/storage/engine.go".into(),
                content: r#"
package storage

type KVEngine struct {
    root string
}

func NewKVEngine(root string) *KVEngine {
    return &KVEngine{root: root}
}
"#
                .into(),
            },
            RepoFileInput {
                path: "cmd/daemon/main.go".into(),
                content: r#"
package main

import "github.com/rursor/app/internal/storage"

func main() {
    _ = storage.NewKVEngine("/tmp")
}
"#
                .into(),
            },
            RepoFileInput {
                path: "internal/misc/unused.go".into(),
                content: "package misc\n\nfunc UnusedHelper() {}\n".into(),
            },
        ];

        let seed = RepoMapSeed {
            active_files: vec!["cmd/daemon/main.go".into()],
            dirty_files: vec![],
            query_terms: vec![],
        };

        let ranked = rank_repo_files(&files, &seed);
        let total_mass: f64 = ranked.iter().map(|r| r.score).sum();
        assert!((total_mass - 1.0).abs() < 1e-9);

        let engine_score = ranked
            .iter()
            .find(|r| r.path == "internal/storage/engine.go")
            .unwrap()
            .score;
        let unused_score = ranked
            .iter()
            .find(|r| r.path == "internal/misc/unused.go")
            .unwrap()
            .score;
        assert!(engine_score > unused_score);

        assert_eq!(build_pagerank_repo_map(&[], &seed, 1000), "");
        assert_eq!(build_pagerank_repo_map(&files, &seed, 0), "");
    }
}
