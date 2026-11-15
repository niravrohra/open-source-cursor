use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use tree_sitter::{Language, Node, Parser};

pub const EMBEDDING_DIM: usize = 128;
const RRF_K: f64 = 60.0;
const MIN_VECTOR_SIMILARITY: f32 = 0.08;
const MAX_CHUNKS_PER_FILE: usize = 512;
const FALLBACK_CHUNK_LINES: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SupportedLanguage {
    Rust,
    TypeScript,
    Tsx,
    JavaScript,
    Python,
    Go,
    Fallback,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AstChunk {
    pub name: String,
    pub kind: String,
    pub start_line: usize,
    pub end_line: usize,
    pub signature: String,
    pub calls: Vec<String>,
    pub imports: Vec<String>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallerHit {
    pub rel_path: String,
    pub caller_name: String,
    pub caller_kind: String,
    pub start_line: usize,
    pub end_line: usize,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HybridSearchHit {
    pub chunk_id: i64,
    pub rel_path: String,
    pub name: String,
    pub kind: String,
    pub start_line: usize,
    pub end_line: usize,
    pub signature: String,
    pub content: String,
    pub rrf_score: f64,
    pub fts_rank: Option<usize>,
    pub vector_rank: Option<usize>,
    pub cosine_sim: Option<f32>,
}

#[derive(Debug, Clone)]
pub struct SyncCandidate {
    pub rel_path: String,
    pub mtime_ns: i64,
    pub abs_path: Option<PathBuf>,
    pub content: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncStats {
    pub skipped: usize,
    pub updated: usize,
    pub removed: usize,
    pub total_chunks: usize,
}

#[derive(Debug)]
pub struct AstIndex {
    conn: Connection,
    #[allow(dead_code)]
    db_path: PathBuf,
}

pub fn detect_language(rel_path: &str) -> SupportedLanguage {
    let ext = Path::new(rel_path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());

    match ext.as_deref() {
        Some("rs") => SupportedLanguage::Rust,
        Some("ts") | Some("mts") | Some("cts") => SupportedLanguage::TypeScript,
        Some("tsx") => SupportedLanguage::Tsx,
        Some("js") | Some("jsx") | Some("mjs") | Some("cjs") => SupportedLanguage::JavaScript,
        Some("py") | Some("pyi") => SupportedLanguage::Python,
        Some("go") => SupportedLanguage::Go,
        _ => SupportedLanguage::Fallback,
    }
}

fn tree_sitter_language(lang: SupportedLanguage) -> Option<Language> {
    match lang {
        SupportedLanguage::Rust => Some(tree_sitter_rust::LANGUAGE.into()),
        SupportedLanguage::TypeScript => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        SupportedLanguage::Tsx => Some(tree_sitter_typescript::LANGUAGE_TSX.into()),
        SupportedLanguage::JavaScript => Some(tree_sitter_javascript::LANGUAGE.into()),
        SupportedLanguage::Python => Some(tree_sitter_python::LANGUAGE.into()),
        SupportedLanguage::Go => Some(tree_sitter_go::LANGUAGE.into()),
        SupportedLanguage::Fallback => None,
    }
}

/// Deterministic 64-bit FNV-1a hash (stable across processes and platforms).
pub fn fnv1a_64(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut hash = OFFSET_BASIS;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

pub fn default_db_path_for_workspace(workspace_root: &Path) -> PathBuf {
    let canonical = fs::canonicalize(workspace_root).unwrap_or_else(|_| workspace_root.to_path_buf());
    let hash = fnv1a_64(canonical.to_string_lossy().as_bytes());
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home)
        .join(".rursor")
        .join("indexes")
        .join(format!("ws_{hash:016x}.db"))
}

/// Fast deterministic 128-dim normalized trigram + identifier vector embedding.
pub fn compute_local_embedding(text: &str) -> Vec<f32> {
    let mut vec = vec![0.0f32; EMBEDDING_DIM];
    if text.trim().is_empty() {
        return vec;
    }

    let tokens = extract_embedding_tokens(text);
    for token in &tokens {
        // Weight full identifier tokens higher than individual trigrams.
        add_hashed_feature(&mut vec, token.as_bytes(), 2.0);

        let chars: Vec<char> = token.chars().collect();
        if chars.len() >= 3 {
            for window in chars.windows(3) {
                let trigram: String = window.iter().collect();
                add_hashed_feature(&mut vec, trigram.as_bytes(), 1.0);
            }
        } else if chars.len() == 2 {
            add_hashed_feature(&mut vec, token.as_bytes(), 1.0);
        }
    }

    // Also include raw lowercased 3-char sliding windows across word boundaries for fuzzy recall.
    let lower_chars: Vec<char> = text
        .chars()
        .take(4096)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    for window in lower_chars.windows(3) {
        if window.iter().all(|c| !c.is_whitespace()) {
            let mut buf = [0u8; 12];
            let mut len = 0;
            for &ch in window {
                let encoded = ch.encode_utf8(&mut buf[len..]);
                len += encoded.len();
            }
            add_hashed_feature(&mut vec, &buf[..len], 0.35);
        }
    }

    l2_normalize(&mut vec);
    vec
}

fn add_hashed_feature(vec: &mut [f32], feature: &[u8], weight: f32) {
    let h = fnv1a_64(feature);
    let idx = (h as usize) % EMBEDDING_DIM;
    let sign = if ((h >> 16) & 1) == 0 { 1.0 } else { -1.0 };
    vec[idx] += sign * weight;
}

fn l2_normalize(vec: &mut [f32]) {
    let norm_sq: f32 = vec.iter().map(|v| v * v).sum();
    if norm_sq > 0.0 {
        let inv_norm = norm_sq.sqrt().recip();
        for v in vec.iter_mut() {
            *v *= inv_norm;
        }
    }
}

pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        norm_a += a[i] * a[i];
        norm_b += b[i] * b[i];
    }
    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom <= 1e-12 {
        0.0
    } else {
        (dot / denom).clamp(-1.0, 1.0)
    }
}

fn embedding_to_blob(embedding: &[f32]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(embedding.len() * 4);
    for &val in embedding {
        blob.extend_from_slice(&val.to_le_bytes());
    }
    blob
}

fn blob_to_embedding(blob: &[u8]) -> Option<Vec<f32>> {
    if blob.len() != EMBEDDING_DIM * 4 {
        return None;
    }
    let mut out = Vec::with_capacity(EMBEDDING_DIM);
    for chunk in blob.chunks_exact(4) {
        out.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Some(out)
}

fn extract_embedding_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in text.split(|c: char| !c.is_alphanumeric() && c != '_') {
        if raw.is_empty() {
            continue;
        }
        let lower = raw.to_lowercase();
        out.push(lower);
        for sub in split_identifier_parts(raw) {
            if sub.len() >= 2 {
                out.push(sub);
            }
        }
    }
    out
}

fn split_identifier_parts(ident: &str) -> Vec<String> {
    let mut parts = Vec::new();
    for seg in ident.split('_') {
        if seg.is_empty() {
            continue;
        }
        let mut current = String::new();
        let chars: Vec<char> = seg.chars().collect();
        for (i, &ch) in chars.iter().enumerate() {
            let is_boundary = i > 0
                && ch.is_uppercase()
                && (chars[i - 1].is_lowercase()
                    || (i + 1 < chars.len() && chars[i + 1].is_lowercase()));
            if is_boundary && !current.is_empty() {
                parts.push(current.to_lowercase());
                current.clear();
            }
            current.push(ch);
        }
        if !current.is_empty() {
            parts.push(current.to_lowercase());
        }
    }
    parts
}

pub fn parse_chunks(rel_path: &str, source: &str) -> Vec<AstChunk> {
    parse_file_ast(rel_path, source).1
}

pub fn parse_file_ast(rel_path: &str, source: &str) -> (Vec<String>, Vec<AstChunk>) {
    if source.trim().is_empty() {
        return (Vec::new(), Vec::new());
    }

    let lang = detect_language(rel_path);
    if let Some(ts_lang) = tree_sitter_language(lang) {
        let mut parser = Parser::new();
        if parser.set_language(&ts_lang).is_ok() {
            if let Some(tree) = parser.parse(source, None) {
                let root = tree.root_node();
                let mut imports = Vec::new();
                collect_imports(root, source, lang, &mut imports);
                dedup_preserve_order(&mut imports);

                let mut chunks = Vec::new();
                collect_ast_chunks(root, source, lang, &imports, false, &mut chunks);
                chunks.truncate(MAX_CHUNKS_PER_FILE);

                if !chunks.is_empty() {
                    return (imports, chunks);
                }

                // Supported file with only top-level expressions/statements (no named decls).
                let file_calls = collect_calls_in_subtree(root, source, lang, false);
                let line_count = source.lines().count().max(1);
                let file_name = Path::new(rel_path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(rel_path)
                    .to_string();
                return (
                    imports.clone(),
                    vec![AstChunk {
                        name: file_name.clone(),
                        kind: "module".to_string(),
                        start_line: 1,
                        end_line: line_count,
                        signature: file_name,
                        calls: file_calls,
                        imports,
                        text: truncate_chunk_text(source),
                    }],
                );
            }
        }
    }

    let chunks = parse_fallback_chunks(rel_path, source);
    (Vec::new(), chunks)
}

fn node_text<'a>(node: Node, source: &'a str) -> &'a str {
    source.get(node.byte_range()).unwrap_or("")
}

fn collect_imports(node: Node, source: &str, lang: SupportedLanguage, out: &mut Vec<String>) {
    let is_import = match lang {
        SupportedLanguage::Rust => node.kind() == "use_declaration",
        SupportedLanguage::TypeScript
        | SupportedLanguage::Tsx
        | SupportedLanguage::JavaScript => node.kind() == "import_statement",
        SupportedLanguage::Python => {
            node.kind() == "import_statement" || node.kind() == "import_from_statement"
        }
        SupportedLanguage::Go => node.kind() == "import_declaration",
        SupportedLanguage::Fallback => false,
    };

    if is_import {
        let raw = collapse_ws(node_text(node, source));
        if !raw.is_empty() {
            out.push(raw);
        }
        return;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_imports(child, source, lang, out);
    }
}

fn collect_ast_chunks(
    node: Node,
    source: &str,
    lang: SupportedLanguage,
    imports: &[String],
    inside_type_container: bool,
    out: &mut Vec<AstChunk>,
) {
    if out.len() >= MAX_CHUNKS_PER_FILE {
        return;
    }

    match lang {
        SupportedLanguage::Rust => {
            collect_rust_node(node, source, imports, inside_type_container, out)
        }
        SupportedLanguage::TypeScript
        | SupportedLanguage::Tsx
        | SupportedLanguage::JavaScript => {
            collect_js_ts_node(node, source, lang, imports, inside_type_container, out)
        }
        SupportedLanguage::Python => {
            collect_python_node(node, source, imports, inside_type_container, out)
        }
        SupportedLanguage::Go => collect_go_node(node, source, imports, out),
        SupportedLanguage::Fallback => {}
    }
}

fn collect_rust_node(
    node: Node,
    source: &str,
    imports: &[String],
    inside_container: bool,
    out: &mut Vec<AstChunk>,
) {
    let kind_str = node.kind();
    match kind_str {
        "function_item" | "function_signature_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, source).trim().to_string();
                if !name.is_empty() {
                    let kind = if inside_container { "method" } else { "fn" };
                    let sig = extract_signature(node, source, &["body"]);
                    let calls = collect_calls_in_subtree(node, source, SupportedLanguage::Rust, true);
                    out.push(build_chunk(node, source, name, kind, sig, calls, imports));
                }
            }
            return;
        }
        "struct_item" | "enum_item" | "union_item" | "type_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, source).trim().to_string();
                let kind = match kind_str {
                    "struct_item" => "struct",
                    "enum_item" => "enum",
                    "union_item" => "union",
                    _ => "type",
                };
                let sig = extract_signature(node, source, &["body"]);
                out.push(build_chunk(node, source, name, kind, sig, Vec::new(), imports));
            }
            return;
        }
        "trait_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, source).trim().to_string();
                let sig = extract_signature(node, source, &["body"]);
                out.push(build_chunk(node, source, name, "trait", sig, Vec::new(), imports));
            }
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.children(&mut cursor) {
                    collect_rust_node(child, source, imports, true, out);
                }
            }
            return;
        }
        "impl_item" => {
            let type_name = node
                .child_by_field_name("type")
                .map(|n| collapse_ws(node_text(n, source)))
                .unwrap_or_default();
            let trait_name = node
                .child_by_field_name("trait")
                .map(|n| collapse_ws(node_text(n, source)));
            let name = match trait_name {
                Some(tr) if !tr.is_empty() && !type_name.is_empty() => format!("{tr} for {type_name}"),
                _ => type_name,
            };
            if !name.is_empty() {
                let sig = extract_signature(node, source, &["body"]);
                out.push(build_chunk(node, source, name, "impl", sig, Vec::new(), imports));
            }
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.children(&mut cursor) {
                    collect_rust_node(child, source, imports, true, out);
                }
            }
            return;
        }
        "mod_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, source).trim().to_string();
                let sig = extract_signature(node, source, &["body"]);
                out.push(build_chunk(node, source, name, "mod", sig, Vec::new(), imports));
            }
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.children(&mut cursor) {
                    collect_rust_node(child, source, imports, false, out);
                }
            }
            return;
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_rust_node(child, source, imports, inside_container, out);
    }
}

fn collect_js_ts_node(
    node: Node,
    source: &str,
    lang: SupportedLanguage,
    imports: &[String],
    inside_class: bool,
    out: &mut Vec<AstChunk>,
) {
    match node.kind() {
        "function_declaration" | "generator_function_declaration" | "function_signature" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, source).trim().to_string();
                let sig = extract_signature(node, source, &["body"]);
                let calls = collect_calls_in_subtree(node, source, lang, true);
                out.push(build_chunk(node, source, name, "function", sig, calls, imports));
            }
            return;
        }
        "method_definition" | "method_signature" | "abstract_method_signature" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, source).trim().to_string();
                let sig = extract_signature(node, source, &["body"]);
                let calls = collect_calls_in_subtree(node, source, lang, true);
                out.push(build_chunk(node, source, name, "method", sig, calls, imports));
            }
            return;
        }
        "class_declaration" | "abstract_class_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, source).trim().to_string();
                let sig = extract_signature(node, source, &["body"]);
                out.push(build_chunk(node, source, name, "class", sig, Vec::new(), imports));
            }
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.children(&mut cursor) {
                    collect_js_ts_node(child, source, lang, imports, true, out);
                }
            }
            return;
        }
        "interface_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, source).trim().to_string();
                let sig = extract_signature(node, source, &["body"]);
                out.push(build_chunk(node, source, name, "interface", sig, Vec::new(), imports));
            }
            return;
        }
        "type_alias_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, source).trim().to_string();
                let sig = extract_signature(node, source, &["value"]);
                out.push(build_chunk(node, source, name, "type", sig, Vec::new(), imports));
            }
            return;
        }
        "enum_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, source).trim().to_string();
                let sig = extract_signature(node, source, &["body"]);
                out.push(build_chunk(node, source, name, "enum", sig, Vec::new(), imports));
            }
            return;
        }
        "lexical_declaration" | "variable_declaration" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "variable_declarator" {
                    if let (Some(name_node), Some(value_node)) = (
                        child.child_by_field_name("name"),
                        child.child_by_field_name("value"),
                    ) {
                        if matches!(
                            value_node.kind(),
                            "arrow_function" | "function_expression" | "generator_function"
                        ) {
                            let name = node_text(name_node, source).trim().to_string();
                            let sig = extract_signature(node, source, &["body"]);
                            let calls = collect_calls_in_subtree(value_node, source, lang, true);
                            out.push(build_chunk(
                                node, source, name, "function", sig, calls, imports,
                            ));
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
        collect_js_ts_node(child, source, lang, imports, inside_class, out);
    }
}

fn collect_python_node(
    node: Node,
    source: &str,
    imports: &[String],
    inside_class: bool,
    out: &mut Vec<AstChunk>,
) {
    match node.kind() {
        "decorated_definition" => {
            if let Some(def) = node.child_by_field_name("definition") {
                match def.kind() {
                    "function_definition" => {
                        if let Some(name_node) = def.child_by_field_name("name") {
                            let name = node_text(name_node, source).trim().to_string();
                            let kind = if inside_class { "method" } else { "function" };
                            let sig = extract_signature(def, source, &["body"]);
                            let calls = collect_calls_in_subtree(
                                def,
                                source,
                                SupportedLanguage::Python,
                                true,
                            );
                            out.push(build_chunk(node, source, name, kind, sig, calls, imports));
                        }
                        return;
                    }
                    "class_definition" => {
                        if let Some(name_node) = def.child_by_field_name("name") {
                            let name = node_text(name_node, source).trim().to_string();
                            let sig = extract_signature(def, source, &["body"]);
                            out.push(build_chunk(
                                node,
                                source,
                                name,
                                "class",
                                sig,
                                Vec::new(),
                                imports,
                            ));
                        }
                        if let Some(body) = def.child_by_field_name("body") {
                            let mut cursor = body.walk();
                            for child in body.children(&mut cursor) {
                                collect_python_node(child, source, imports, true, out);
                            }
                        }
                        return;
                    }
                    _ => {}
                }
            }
        }
        "function_definition" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, source).trim().to_string();
                let kind = if inside_class { "method" } else { "function" };
                let sig = extract_signature(node, source, &["body"]);
                let calls =
                    collect_calls_in_subtree(node, source, SupportedLanguage::Python, true);
                out.push(build_chunk(node, source, name, kind, sig, calls, imports));
            }
            return;
        }
        "class_definition" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, source).trim().to_string();
                let sig = extract_signature(node, source, &["body"]);
                out.push(build_chunk(node, source, name, "class", sig, Vec::new(), imports));
            }
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.children(&mut cursor) {
                    collect_python_node(child, source, imports, true, out);
                }
            }
            return;
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_python_node(child, source, imports, inside_class, out);
    }
}

fn collect_go_node(
    node: Node,
    source: &str,
    imports: &[String],
    out: &mut Vec<AstChunk>,
) {
    match node.kind() {
        "function_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, source).trim().to_string();
                let sig = extract_signature(node, source, &["body"]);
                let calls = collect_calls_in_subtree(node, source, SupportedLanguage::Go, true);
                out.push(build_chunk(node, source, name, "function", sig, calls, imports));
            }
            return;
        }
        "method_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, source).trim().to_string();
                let sig = extract_signature(node, source, &["body"]);
                let calls = collect_calls_in_subtree(node, source, SupportedLanguage::Go, true);
                out.push(build_chunk(node, source, name, "method", sig, calls, imports));
            }
            return;
        }
        "type_declaration" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "type_spec" {
                    if let Some(name_node) = child.child_by_field_name("name") {
                        let name = node_text(name_node, source).trim().to_string();
                        let kind = match child.child_by_field_name("type").map(|t| t.kind()) {
                            Some("struct_type") => "struct",
                            Some("interface_type") => "interface",
                            _ => "type",
                        };
                        let sig = extract_signature(child, source, &[]);
                        out.push(build_chunk(
                            child,
                            source,
                            name,
                            kind,
                            sig,
                            Vec::new(),
                            imports,
                        ));
                    }
                }
            }
            return;
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_go_node(child, source, imports, out);
    }
}

fn collect_calls_in_subtree(
    root: Node,
    source: &str,
    lang: SupportedLanguage,
    skip_nested_decls: bool,
) -> Vec<String> {
    let mut calls = Vec::new();
    collect_calls_recursive(root, root, source, lang, skip_nested_decls, &mut calls);
    dedup_preserve_order(&mut calls);
    calls
}

fn collect_calls_recursive(
    root: Node,
    node: Node,
    source: &str,
    lang: SupportedLanguage,
    skip_nested_decls: bool,
    out: &mut Vec<String>,
) {
    if skip_nested_decls && node.id() != root.id() && is_named_container_or_fn(node.kind()) {
        return;
    }

    match lang {
        SupportedLanguage::Rust => {
            if node.kind() == "call_expression" {
                if let Some(func) = node.child_by_field_name("function") {
                    extract_rust_callee(func, source, out);
                }
            } else if node.kind() == "macro_invocation" {
                if let Some(mac) = node.child_by_field_name("macro") {
                    let text = node_text(mac, source).trim();
                    if let Some(leaf) = text.rsplit("::").next() {
                        if !leaf.is_empty() {
                            out.push(format!("{leaf}!"));
                        }
                    }
                }
            }
        }
        SupportedLanguage::TypeScript
        | SupportedLanguage::Tsx
        | SupportedLanguage::JavaScript => {
            if node.kind() == "call_expression" {
                if let Some(func) = node.child_by_field_name("function") {
                    extract_js_callee(func, source, out);
                }
            } else if node.kind() == "new_expression" {
                if let Some(ctor) = node.child_by_field_name("constructor") {
                    extract_js_callee(ctor, source, out);
                }
            }
        }
        SupportedLanguage::Python => {
            if node.kind() == "call" {
                if let Some(func) = node.child_by_field_name("function") {
                    extract_python_callee(func, source, out);
                }
            }
        }
        SupportedLanguage::Go => {
            if node.kind() == "call_expression" {
                if let Some(func) = node.child_by_field_name("function") {
                    extract_go_callee(func, source, out);
                }
            }
        }
        SupportedLanguage::Fallback => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_calls_recursive(root, child, source, lang, skip_nested_decls, out);
    }
}

fn is_named_container_or_fn(kind: &str) -> bool {
    matches!(
        kind,
        "function_item"
            | "impl_item"
            | "trait_item"
            | "function_declaration"
            | "generator_function_declaration"
            | "method_definition"
            | "class_declaration"
            | "abstract_class_declaration"
            | "function_definition"
            | "class_definition"
            | "method_declaration"
    )
}

fn extract_rust_callee(node: Node, source: &str, out: &mut Vec<String>) {
    match node.kind() {
        "identifier" => {
            let name = node_text(node, source).trim();
            if !name.is_empty() {
                out.push(name.to_string());
            }
        }
        "field_expression" => {
            if let Some(field) = node.child_by_field_name("field") {
                let name = node_text(field, source).trim();
                if !name.is_empty() {
                    out.push(name.to_string());
                }
            }
        }
        "scoped_identifier" => {
            let full = collapse_ws(node_text(node, source));
            if let Some(name_node) = node.child_by_field_name("name") {
                let leaf = node_text(name_node, source).trim();
                if !leaf.is_empty() {
                    out.push(leaf.to_string());
                }
            }
            if full.contains("::") && !full.contains(' ') {
                out.push(full);
            }
        }
        "generic_function" => {
            if let Some(inner) = node.child_by_field_name("function") {
                extract_rust_callee(inner, source, out);
            }
        }
        _ => {}
    }
}

fn extract_js_callee(node: Node, source: &str, out: &mut Vec<String>) {
    match node.kind() {
        "identifier" => {
            let name = node_text(node, source).trim();
            if !name.is_empty() {
                out.push(name.to_string());
            }
        }
        "member_expression" => {
            if let Some(prop) = node.child_by_field_name("property") {
                let name = node_text(prop, source).trim();
                if !name.is_empty() {
                    out.push(name.to_string());
                }
            }
        }
        "parenthesized_expression" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_js_callee(child, source, out);
            }
        }
        _ => {}
    }
}

fn extract_python_callee(node: Node, source: &str, out: &mut Vec<String>) {
    match node.kind() {
        "identifier" => {
            let name = node_text(node, source).trim();
            if !name.is_empty() {
                out.push(name.to_string());
            }
        }
        "attribute" => {
            if let Some(attr) = node.child_by_field_name("attribute") {
                let name = node_text(attr, source).trim();
                if !name.is_empty() {
                    out.push(name.to_string());
                }
            }
        }
        _ => {}
    }
}

fn extract_go_callee(node: Node, source: &str, out: &mut Vec<String>) {
    match node.kind() {
        "identifier" => {
            let name = node_text(node, source).trim();
            if !name.is_empty() {
                out.push(name.to_string());
            }
        }
        "selector_expression" => {
            if let Some(field) = node.child_by_field_name("field") {
                let name = node_text(field, source).trim();
                if !name.is_empty() {
                    out.push(name.to_string());
                }
            }
        }
        "parenthesized_expression" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_go_callee(child, source, out);
            }
        }
        _ => {}
    }
}

fn extract_signature(node: Node, source: &str, body_fields: &[&str]) -> String {
    let start = node.start_byte();
    let mut sig_end = node.end_byte();
    for &field in body_fields {
        if let Some(body) = node.child_by_field_name(field) {
            sig_end = sig_end.min(body.start_byte());
        }
    }
    let slice = source
        .get(start..sig_end)
        .unwrap_or_else(|| node_text(node, source));
    let first_part = if body_fields.is_empty() {
        slice.lines().next().unwrap_or(slice)
    } else {
        slice
    };
    let cleaned = collapse_ws(first_part);
    let trimmed = cleaned.trim_end_matches(['{', ':', ' ']).trim();
    trimmed.chars().take(220).collect()
}

fn build_chunk(
    node: Node,
    source: &str,
    name: String,
    kind: &str,
    signature: String,
    calls: Vec<String>,
    imports: &[String],
) -> AstChunk {
    let start_line = node.start_position().row + 1;
    let end_line = node.end_position().row + 1;
    let text = truncate_chunk_text(node_text(node, source));
    AstChunk {
        name,
        kind: kind.to_string(),
        start_line,
        end_line,
        signature,
        calls,
        imports: imports.to_vec(),
        text,
    }
}

fn truncate_chunk_text(text: &str) -> String {
    if text.len() <= 8_000 {
        text.to_string()
    } else {
        text.chars().take(8_000).collect()
    }
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn dedup_preserve_order(items: &mut Vec<String>) {
    let mut seen = HashSet::new();
    items.retain(|item| seen.insert(item.clone()));
}

fn parse_fallback_chunks(rel_path: &str, source: &str) -> Vec<AstChunk> {
    let lines: Vec<&str> = source.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }

    let mut chunks = Vec::new();
    let mut offset = 0usize;
    while offset < lines.len() && chunks.len() < MAX_CHUNKS_PER_FILE {
        let end = (offset + FALLBACK_CHUNK_LINES).min(lines.len());
        let slice_text = lines[offset..end].join("\n");
        if !slice_text.trim().is_empty() {
            let mut detected_name = None;
            let mut detected_kind = "chunk";
            for line in &lines[offset..end] {
                if let Some((kind, name)) = detect_fallback_symbol(line) {
                    detected_kind = kind;
                    detected_name = Some(name);
                    break;
                }
            }
            let file_label = Path::new(rel_path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(rel_path);
            let name = detected_name.unwrap_or_else(|| format!("{file_label}:{}", offset + 1));
            let signature = lines[offset..end]
                .iter()
                .find(|l| !l.trim().is_empty())
                .map(|l| l.trim().chars().take(180).collect())
                .unwrap_or_default();

            chunks.push(AstChunk {
                name,
                kind: detected_kind.to_string(),
                start_line: offset + 1,
                end_line: end,
                signature,
                calls: Vec::new(),
                imports: Vec::new(),
                text: truncate_chunk_text(&slice_text),
            });
        }
        offset = end;
    }

    chunks
}

fn detect_fallback_symbol(line: &str) -> Option<(&'static str, String)> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with('#') {
        return None;
    }
    let clean = trimmed
        .strip_prefix("pub(crate) ")
        .or_else(|| trimmed.strip_prefix("pub "))
        .or_else(|| trimmed.strip_prefix("export default "))
        .or_else(|| trimmed.strip_prefix("export "))
        .unwrap_or(trimmed);
    let clean = clean.strip_prefix("async ").unwrap_or(clean);

    let extracted = if let Some(rest) = clean.strip_prefix("fn ") {
        Some(("fn", rest))
    } else if let Some(rest) = clean.strip_prefix("function ") {
        Some(("function", rest))
    } else if let Some(rest) = clean.strip_prefix("def ") {
        Some(("def", rest))
    } else if let Some(rest) = clean.strip_prefix("struct ") {
        Some(("struct", rest))
    } else if let Some(rest) = clean.strip_prefix("enum ") {
        Some(("enum", rest))
    } else if let Some(rest) = clean.strip_prefix("trait ") {
        Some(("trait", rest))
    } else if let Some(rest) = clean.strip_prefix("class ") {
        Some(("class", rest))
    } else if let Some(rest) = clean.strip_prefix("interface ") {
        Some(("interface", rest))
    } else if let Some(rest) = clean.strip_prefix("type ") {
        Some(("type", rest))
    } else if let Some(rest) = clean.strip_prefix("impl ") {
        Some(("impl", rest))
    } else {
        None
    };

    extracted.and_then(|(kind, rest)| {
        let ident: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if ident.len() >= 2 {
            Some((kind, ident))
        } else {
            None
        }
    })
}

impl AstIndex {
    pub fn open_for_workspace(workspace_root: &Path) -> Result<Self, String> {
        let db_path = default_db_path_for_workspace(workspace_root);
        Self::open_at(&db_path)
    }

    pub fn open_at(db_path: &Path) -> Result<Self, String> {
        if let Some(parent) = db_path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("Failed to create index dir {}: {e}", parent.display()))?;
            }
        }
        let conn = Connection::open(db_path)
            .map_err(|e| format!("Failed to open SQLite index {}: {e}", db_path.display()))?;
        let index = Self {
            conn,
            db_path: db_path.to_path_buf(),
        };
        index.init_schema()?;
        Ok(index)
    }

    pub fn open_in_memory() -> Result<Self, String> {
        let conn = Connection::open_in_memory()
            .map_err(|e| format!("Failed to open in-memory SQLite index: {e}"))?;
        let index = Self {
            conn,
            db_path: PathBuf::from(":memory:"),
        };
        index.init_schema()?;
        Ok(index)
    }

    #[allow(dead_code)]
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    fn init_schema(&self) -> Result<(), String> {
        self.conn
            .execute_batch(
                "
                PRAGMA journal_mode = WAL;
                PRAGMA synchronous = NORMAL;
                PRAGMA temp_store = MEMORY;

                CREATE TABLE IF NOT EXISTS files (
                    rel_path TEXT PRIMARY KEY,
                    mtime_ns INTEGER NOT NULL,
                    content_hash INTEGER NOT NULL,
                    imports_json TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS chunks (
                    id INTEGER PRIMARY KEY,
                    rel_path TEXT NOT NULL,
                    name TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    start_line INTEGER NOT NULL,
                    end_line INTEGER NOT NULL,
                    signature TEXT NOT NULL,
                    calls_json TEXT NOT NULL,
                    embedding BLOB NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_chunks_rel_path ON chunks(rel_path);
                CREATE INDEX IF NOT EXISTS idx_chunks_name ON chunks(name);

                CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
                    rel_path,
                    name,
                    kind,
                    signature,
                    content,
                    tokenize='porter unicode61'
                );
                ",
            )
            .map_err(|e| format!("Failed to initialize SQLite AST schema: {e}"))
    }

    pub fn sync_incremental(&mut self, candidates: &[SyncCandidate]) -> Result<SyncStats, String> {
        let mut existing: HashMap<String, (i64, i64)> = HashMap::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT rel_path, mtime_ns, content_hash FROM files")
                .map_err(|e| format!("Failed to read existing files: {e}"))?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })
                .map_err(|e| format!("Failed to query existing files: {e}"))?;
            for row in rows {
                let (rel_path, mtime_ns, content_hash) =
                    row.map_err(|e| format!("Row decode error: {e}"))?;
                existing.insert(rel_path, (mtime_ns, content_hash));
            }
        }

        let tx = self
            .conn
            .transaction()
            .map_err(|e| format!("Failed to start sync transaction: {e}"))?;

        let mut stats = SyncStats::default();
        let mut seen_paths = HashSet::with_capacity(candidates.len());

        for candidate in candidates {
            seen_paths.insert(candidate.rel_path.clone());

            if let Some(&(stored_mtime, stored_hash)) = existing.get(&candidate.rel_path) {
                if stored_mtime == candidate.mtime_ns && candidate.content.is_none() {
                    stats.skipped += 1;
                    continue;
                }

                let content = match load_candidate_content(candidate) {
                    Ok(Some(c)) => c,
                    Ok(None) => continue,
                    Err(_) => continue,
                };
                let new_hash = fnv1a_64(content.as_bytes()) as i64;
                if new_hash == stored_hash {
                    if stored_mtime != candidate.mtime_ns {
                        tx.execute(
                            "UPDATE files SET mtime_ns = ?1 WHERE rel_path = ?2",
                            params![candidate.mtime_ns, candidate.rel_path],
                        )
                        .map_err(|e| format!("Failed to update mtime: {e}"))?;
                    }
                    stats.skipped += 1;
                    continue;
                }

                upsert_file_in_tx(&tx, &candidate.rel_path, candidate.mtime_ns, new_hash, &content)?;
                stats.updated += 1;
            } else {
                let content = match load_candidate_content(candidate) {
                    Ok(Some(c)) => c,
                    Ok(None) => continue,
                    Err(_) => continue,
                };
                let new_hash = fnv1a_64(content.as_bytes()) as i64;
                upsert_file_in_tx(&tx, &candidate.rel_path, candidate.mtime_ns, new_hash, &content)?;
                stats.updated += 1;
            }
        }

        for old_path in existing.keys() {
            if !seen_paths.contains(old_path) {
                delete_file_in_tx(&tx, old_path)?;
                stats.removed += 1;
            }
        }

        let total_chunks: i64 = tx
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
            .map_err(|e| format!("Failed to count chunks: {e}"))?;
        stats.total_chunks = total_chunks.max(0) as usize;

        tx.commit()
            .map_err(|e| format!("Failed to commit sync transaction: {e}"))?;

        Ok(stats)
    }

    pub fn upsert_file(&mut self, rel_path: &str, mtime_ns: i64, content: &str) -> Result<usize, String> {
        let hash = fnv1a_64(content.as_bytes()) as i64;
        let tx = self
            .conn
            .transaction()
            .map_err(|e| format!("Failed to start upsert transaction: {e}"))?;
        let inserted = upsert_file_in_tx(&tx, rel_path, mtime_ns, hash, content)?;
        tx.commit()
            .map_err(|e| format!("Failed to commit upsert transaction: {e}"))?;
        Ok(inserted)
    }

    pub fn remove_file(&mut self, rel_path: &str) -> Result<bool, String> {
        let tx = self
            .conn
            .transaction()
            .map_err(|e| format!("Failed to start remove transaction: {e}"))?;
        let removed = delete_file_in_tx(&tx, rel_path)?;
        tx.commit()
            .map_err(|e| format!("Failed to commit remove transaction: {e}"))?;
        Ok(removed)
    }

    pub fn upsert_chunk_embedding(&self, chunk_id: i64, embedding: &[f32]) -> Result<bool, String> {
        if embedding.len() != EMBEDDING_DIM {
            return Err(format!(
                "Expected {EMBEDDING_DIM}-dim embedding, got {}",
                embedding.len()
            ));
        }
        let mut normalized = embedding.to_vec();
        l2_normalize(&mut normalized);
        let blob = embedding_to_blob(&normalized);
        let changed = self
            .conn
            .execute(
                "UPDATE chunks SET embedding = ?1 WHERE id = ?2",
                params![blob, chunk_id],
            )
            .map_err(|e| format!("Failed to update chunk embedding: {e}"))?;
        Ok(changed > 0)
    }

    /// Hybrid search combining SQLite FTS5 BM25 + 128-dim cosine similarity via Reciprocal Rank Fusion (RRF).
    pub fn search_hybrid(&self, query: &str, limit: usize) -> Result<Vec<HybridSearchHit>, String> {
        if query.trim().is_empty() || limit == 0 {
            return Ok(Vec::new());
        }

        let candidate_limit = (limit * 6).max(30) as i64;
        let mut merged: HashMap<i64, HybridSearchHit> = HashMap::new();

        // 1. SQLite FTS5 BM25 leg.
        if let Some(fts_query) = build_fts_query(query) {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT c.id, c.rel_path, c.name, c.kind, c.start_line, c.end_line, c.signature, chunks_fts.content
                     FROM chunks_fts
                     JOIN chunks c ON c.id = chunks_fts.rowid
                     WHERE chunks_fts MATCH ?1
                     ORDER BY bm25(chunks_fts, 2.0, 4.0, 1.0, 2.5, 1.0) ASC
                     LIMIT ?2",
                )
                .map_err(|e| format!("Failed to prepare FTS5 search: {e}"))?;

            let rows = stmt
                .query_map(params![fts_query, candidate_limit], |row| {
                    Ok(HybridSearchHit {
                        chunk_id: row.get(0)?,
                        rel_path: row.get(1)?,
                        name: row.get(2)?,
                        kind: row.get(3)?,
                        start_line: row.get::<_, i64>(4)?.max(1) as usize,
                        end_line: row.get::<_, i64>(5)?.max(1) as usize,
                        signature: row.get(6)?,
                        content: row.get(7)?,
                        rrf_score: 0.0,
                        fts_rank: None,
                        vector_rank: None,
                        cosine_sim: None,
                    })
                })
                .map_err(|e| format!("Failed to run FTS5 search: {e}"))?;

            for (idx, row) in rows.enumerate() {
                let mut hit = row.map_err(|e| format!("FTS5 row error: {e}"))?;
                let rank = idx + 1;
                hit.fts_rank = Some(rank);
                hit.rrf_score += 1.2 / (RRF_K + rank as f64);
                merged.insert(hit.chunk_id, hit);
            }
        }

        // 2. Vector cosine similarity leg.
        let query_vec = compute_local_embedding(query);
        let has_query_signal = query_vec.iter().any(|&v| v.abs() > 1e-6);
        if has_query_signal {
            let mut vec_candidates: Vec<(i64, f32)> = Vec::new();
            let mut stmt = self
                .conn
                .prepare("SELECT id, embedding FROM chunks")
                .map_err(|e| format!("Failed to prepare vector scan: {e}"))?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
                })
                .map_err(|e| format!("Failed to scan chunk embeddings: {e}"))?;

            for row in rows {
                let (id, blob) = row.map_err(|e| format!("Vector row error: {e}"))?;
                if let Some(emb) = blob_to_embedding(&blob) {
                    let sim = cosine_similarity(&query_vec, &emb);
                    if sim >= MIN_VECTOR_SIMILARITY {
                        vec_candidates.push((id, sim));
                    }
                }
            }

            vec_candidates.sort_by(|a, b| b.1.total_cmp(&a.1));
            vec_candidates.truncate(candidate_limit as usize);

            for (idx, (chunk_id, sim)) in vec_candidates.into_iter().enumerate() {
                let rank = idx + 1;
                let rrf_contrib = 1.0 / (RRF_K + rank as f64);
                if let Some(existing) = merged.get_mut(&chunk_id) {
                    existing.vector_rank = Some(rank);
                    existing.cosine_sim = Some(sim);
                    existing.rrf_score += rrf_contrib;
                } else if let Some(mut hit) = self.fetch_chunk_by_id(chunk_id)? {
                    hit.vector_rank = Some(rank);
                    hit.cosine_sim = Some(sim);
                    hit.rrf_score = rrf_contrib;
                    merged.insert(chunk_id, hit);
                }
            }
        }

        let mut results: Vec<HybridSearchHit> = merged.into_values().collect();
        results.sort_by(|a, b| {
            b.rrf_score
                .total_cmp(&a.rrf_score)
                .then_with(|| a.fts_rank.unwrap_or(usize::MAX).cmp(&b.fts_rank.unwrap_or(usize::MAX)))
                .then_with(|| {
                    b.cosine_sim
                        .unwrap_or(0.0)
                        .total_cmp(&a.cosine_sim.unwrap_or(0.0))
                })
        });
        results.truncate(limit);
        Ok(results)
    }

    fn fetch_chunk_by_id(&self, chunk_id: i64) -> Result<Option<HybridSearchHit>, String> {
        self.conn
            .query_row(
                "SELECT c.id, c.rel_path, c.name, c.kind, c.start_line, c.end_line, c.signature,
                        COALESCE(chunks_fts.content, c.signature)
                 FROM chunks c
                 LEFT JOIN chunks_fts ON chunks_fts.rowid = c.id
                 WHERE c.id = ?1",
                params![chunk_id],
                |row| {
                    Ok(HybridSearchHit {
                        chunk_id: row.get(0)?,
                        rel_path: row.get(1)?,
                        name: row.get(2)?,
                        kind: row.get(3)?,
                        start_line: row.get::<_, i64>(4)?.max(1) as usize,
                        end_line: row.get::<_, i64>(5)?.max(1) as usize,
                        signature: row.get(6)?,
                        content: row.get(7)?,
                        rrf_score: 0.0,
                        fts_rank: None,
                        vector_rank: None,
                        cosine_sim: None,
                    })
                },
            )
            .optional()
            .map_err(|e| format!("Failed to load chunk {chunk_id}: {e}"))
    }

    pub fn find_callers(&self, symbol_name: &str, limit: usize) -> Result<Vec<CallerHit>, String> {
        let needle = symbol_name.trim();
        if needle.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }

        let like_pattern = format!("%{}%", escape_like(needle));
        let mut stmt = self
            .conn
            .prepare(
                "SELECT rel_path, name, kind, start_line, end_line, signature, calls_json
                 FROM chunks
                 WHERE calls_json LIKE ?1 ESCAPE '\\'
                 ORDER BY rel_path ASC, start_line ASC",
            )
            .map_err(|e| format!("Failed to prepare find_callers query: {e}"))?;

        let rows = stmt
            .query_map(params![like_pattern], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?.max(1) as usize,
                    row.get::<_, i64>(4)?.max(1) as usize,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                ))
            })
            .map_err(|e| format!("Failed to query callers: {e}"))?;

        let mut hits = Vec::new();
        for row in rows {
            let (rel_path, caller_name, caller_kind, start_line, end_line, signature, calls_json) =
                row.map_err(|e| format!("Caller row decode error: {e}"))?;
            let calls: Vec<String> = serde_json::from_str(&calls_json).unwrap_or_default();
            let matched = calls.iter().any(|call| {
                call == needle
                    || call.rsplit("::").next() == Some(needle)
                    || call.rsplit('.').next() == Some(needle)
            });
            if matched {
                hits.push(CallerHit {
                    rel_path,
                    caller_name,
                    caller_kind,
                    start_line,
                    end_line,
                    signature,
                });
                if hits.len() >= limit {
                    break;
                }
            }
        }
        Ok(hits)
    }

    pub fn file_outline(&self, rel_path: &str) -> Result<Vec<AstChunk>, String> {
        let imports: Vec<String> = self
            .conn
            .query_row(
                "SELECT imports_json FROM files WHERE rel_path = ?1",
                params![rel_path],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|e| format!("Failed to query file imports: {e}"))?
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default();

        let mut stmt = self
            .conn
            .prepare(
                "SELECT c.name, c.kind, c.start_line, c.end_line, c.signature, c.calls_json,
                        COALESCE(chunks_fts.content, '')
                 FROM chunks c
                 LEFT JOIN chunks_fts ON chunks_fts.rowid = c.id
                 WHERE c.rel_path = ?1
                 ORDER BY c.start_line ASC, c.id ASC",
            )
            .map_err(|e| format!("Failed to prepare file_outline query: {e}"))?;

        let rows = stmt
            .query_map(params![rel_path], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?.max(1) as usize,
                    row.get::<_, i64>(3)?.max(1) as usize,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                ))
            })
            .map_err(|e| format!("Failed to run file_outline query: {e}"))?;

        let mut out = Vec::new();
        for row in rows {
            let (name, kind, start_line, end_line, signature, calls_json, text) =
                row.map_err(|e| format!("Outline row error: {e}"))?;
            let calls: Vec<String> = serde_json::from_str(&calls_json).unwrap_or_default();
            out.push(AstChunk {
                name,
                kind,
                start_line,
                end_line,
                signature,
                calls,
                imports: imports.clone(),
                text,
            });
        }
        Ok(out)
    }

    pub fn load_all_symbols(&self) -> Result<HashMap<String, Vec<(String, String, usize)>>, String> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT rel_path, name, kind, start_line
                 FROM chunks
                 WHERE kind != 'chunk' AND kind != 'module'
                 ORDER BY rel_path ASC, start_line ASC",
            )
            .map_err(|e| format!("Failed to prepare load_all_symbols: {e}"))?;

        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?.max(1) as usize,
                ))
            })
            .map_err(|e| format!("Failed to query symbols: {e}"))?;

        let mut map: HashMap<String, Vec<(String, String, usize)>> = HashMap::new();
        for row in rows {
            let (rel_path, name, kind, line) = row.map_err(|e| format!("Symbol row error: {e}"))?;
            map.entry(rel_path).or_default().push((name, kind, line));
        }
        Ok(map)
    }

    #[allow(dead_code)]
    pub fn chunk_count(&self) -> Result<usize, String> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
            .map_err(|e| format!("Failed to count chunks: {e}"))?;
        Ok(count.max(0) as usize)
    }

    #[allow(dead_code)]
    pub fn file_count(&self) -> Result<usize, String> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .map_err(|e| format!("Failed to count files: {e}"))?;
        Ok(count.max(0) as usize)
    }
}

fn load_candidate_content(candidate: &SyncCandidate) -> Result<Option<String>, String> {
    if let Some(content) = &candidate.content {
        return Ok(Some(content.clone()));
    }
    let Some(abs_path) = &candidate.abs_path else {
        return Ok(None);
    };
    let bytes = fs::read(abs_path).map_err(|e| format!("Failed to read {}: {e}", abs_path.display()))?;
    if bytes.contains(&0) {
        return Ok(None);
    }
    Ok(String::from_utf8(bytes).ok())
}

fn delete_file_in_tx(tx: &rusqlite::Transaction<'_>, rel_path: &str) -> Result<bool, String> {
    tx.execute(
        "DELETE FROM chunks_fts WHERE rowid IN (SELECT id FROM chunks WHERE rel_path = ?1)",
        params![rel_path],
    )
    .map_err(|e| format!("Failed to delete FTS chunks for {rel_path}: {e}"))?;
    tx.execute("DELETE FROM chunks WHERE rel_path = ?1", params![rel_path])
        .map_err(|e| format!("Failed to delete chunks for {rel_path}: {e}"))?;
    let removed = tx
        .execute("DELETE FROM files WHERE rel_path = ?1", params![rel_path])
        .map_err(|e| format!("Failed to delete file row for {rel_path}: {e}"))?;
    Ok(removed > 0)
}

fn upsert_file_in_tx(
    tx: &rusqlite::Transaction<'_>,
    rel_path: &str,
    mtime_ns: i64,
    content_hash: i64,
    content: &str,
) -> Result<usize, String> {
    delete_file_in_tx(tx, rel_path)?;

    let (imports, chunks) = parse_file_ast(rel_path, content);
    let imports_json = serde_json::to_string(&imports).unwrap_or_else(|_| "[]".to_string());

    tx.execute(
        "INSERT INTO files (rel_path, mtime_ns, content_hash, imports_json)
         VALUES (?1, ?2, ?3, ?4)",
        params![rel_path, mtime_ns, content_hash, imports_json],
    )
    .map_err(|e| format!("Failed to insert file {rel_path}: {e}"))?;

    let mut insert_chunk = tx
        .prepare_cached(
            "INSERT INTO chunks (rel_path, name, kind, start_line, end_line, signature, calls_json, embedding)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )
        .map_err(|e| format!("Failed to prepare chunk insert: {e}"))?;
    let mut insert_fts = tx
        .prepare_cached(
            "INSERT INTO chunks_fts (rowid, rel_path, name, kind, signature, content)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .map_err(|e| format!("Failed to prepare FTS insert: {e}"))?;

    for chunk in &chunks {
        let calls_json = serde_json::to_string(&chunk.calls).unwrap_or_else(|_| "[]".to_string());
        let embed_input = format!(
            "{} {} {}\n{}\n{}",
            rel_path, chunk.kind, chunk.name, chunk.signature, chunk.text
        );
        let embedding = compute_local_embedding(&embed_input);
        let blob = embedding_to_blob(&embedding);

        insert_chunk
            .execute(params![
                rel_path,
                chunk.name,
                chunk.kind,
                chunk.start_line as i64,
                chunk.end_line as i64,
                chunk.signature,
                calls_json,
                blob
            ])
            .map_err(|e| format!("Failed to insert chunk {}: {e}", chunk.name))?;

        let chunk_id = tx.last_insert_rowid();
        insert_fts
            .execute(params![
                chunk_id,
                rel_path,
                chunk.name,
                chunk.kind,
                chunk.signature,
                chunk.text
            ])
            .map_err(|e| format!("Failed to insert FTS chunk {}: {e}", chunk.name))?;
    }

    Ok(chunks.len())
}

fn build_fts_query(query: &str) -> Option<String> {
    let mut terms = Vec::new();
    let mut seen = HashSet::new();

    for raw in query.split(|c: char| !c.is_alphanumeric()) {
        if raw.len() < 2 {
            continue;
        }
        let lower = raw.to_lowercase();
        if seen.insert(lower.clone()) {
            terms.push(format!("\"{lower}\"*"));
        }
        for sub in split_identifier_parts(raw) {
            if sub.len() >= 2 && seen.insert(sub.clone()) {
                terms.push(format!("\"{sub}\"*"));
            }
        }
        if terms.len() >= 16 {
            break;
        }
    }

    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" OR "))
    }
}

fn escape_like(input: &str) -> String {
    input
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_db_path(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("rursor_ast_test_{label}_{nanos}.db"))
    }

    #[test]
    fn tree_sitter_parses_rust_ts_python_go() {
        let rust_src = r#"
use std::collections::HashMap;

pub struct AuthService {
    secret: String,
}

pub trait Verifier {
    fn verify(&self, token: &str) -> bool;
}

impl AuthService {
    pub fn verify_token(&self, token: &str) -> bool {
        validate_signature(token, &self.secret)
    }
}

fn validate_signature(token: &str, secret: &str) -> bool {
    !token.is_empty() && !secret.is_empty()
}
"#;
        let (rust_imports, rust_chunks) = parse_file_ast("src/auth.rs", rust_src);
        assert!(rust_imports.iter().any(|i| i.contains("HashMap")));
        assert!(rust_chunks
            .iter()
            .any(|c| c.name == "AuthService" && c.kind == "struct"));
        assert!(rust_chunks
            .iter()
            .any(|c| c.name == "Verifier" && c.kind == "trait"));
        let verify_chunk = rust_chunks
            .iter()
            .find(|c| c.name == "verify_token" && c.kind == "method")
            .expect("verify_token method");
        assert!(verify_chunk.calls.contains(&"validate_signature".to_string()));

        let ts_src = r#"
import { User } from "./types";

export interface AuthConfig {
    issuer: string;
}

export class TokenClient {
    async fetchToken(userId: string): Promise<string> {
        return requestJwt(userId);
    }
}

export const refreshSession = (id: string): boolean => {
    return checkExpiry(id);
};
"#;
        let (ts_imports, ts_chunks) = parse_file_ast("src/client.ts", ts_src);
        assert!(ts_imports.iter().any(|i| i.contains("./types")));
        assert!(ts_chunks
            .iter()
            .any(|c| c.name == "AuthConfig" && c.kind == "interface"));
        assert!(ts_chunks
            .iter()
            .any(|c| c.name == "TokenClient" && c.kind == "class"));
        let fetch_chunk = ts_chunks
            .iter()
            .find(|c| c.name == "fetchToken" && c.kind == "method")
            .expect("fetchToken method");
        assert!(fetch_chunk.calls.contains(&"requestJwt".to_string()));
        let refresh_chunk = ts_chunks
            .iter()
            .find(|c| c.name == "refreshSession" && c.kind == "function")
            .expect("refreshSession arrow fn");
        assert!(refresh_chunk.calls.contains(&"checkExpiry".to_string()));

        let py_src = r#"
from hashlib import sha256

class SessionManager:
    def create_session(self, user_id: str) -> str:
        return sign_payload(user_id)

def sign_payload(payload: str) -> str:
    return sha256(payload.encode()).hexdigest()
"#;
        let (py_imports, py_chunks) = parse_file_ast("app/session.py", py_src);
        assert!(py_imports.iter().any(|i| i.contains("sha256")));
        assert!(py_chunks
            .iter()
            .any(|c| c.name == "SessionManager" && c.kind == "class"));
        let create_chunk = py_chunks
            .iter()
            .find(|c| c.name == "create_session" && c.kind == "method")
            .expect("create_session method");
        assert!(create_chunk.calls.contains(&"sign_payload".to_string()));

        let go_src = r#"
package auth

import "context"

type Server struct {
    addr string
}

type Handler interface {
    Serve(ctx context.Context) error
}

func (s *Server) Start(ctx context.Context) error {
    return listenAndServe(ctx, s.addr)
}
"#;
        let (go_imports, go_chunks) = parse_file_ast("pkg/server.go", go_src);
        assert!(go_imports.iter().any(|i| i.contains("context")));
        assert!(go_chunks
            .iter()
            .any(|c| c.name == "Server" && c.kind == "struct"));
        assert!(go_chunks
            .iter()
            .any(|c| c.name == "Handler" && c.kind == "interface"));
        let start_chunk = go_chunks
            .iter()
            .find(|c| c.name == "Start" && c.kind == "method")
            .expect("Start method");
        assert!(start_chunk.calls.contains(&"listenAndServe".to_string()));

        let tsx_src = r#"
import React from "react";

export function Badge({ label }: { label: string }) {
    return renderTag(label);
}
"#;
        let (tsx_imports, tsx_chunks) = parse_file_ast("src/Badge.tsx", tsx_src);
        assert!(tsx_imports.iter().any(|i| i.contains("react")));
        let badge_chunk = tsx_chunks
            .iter()
            .find(|c| c.name == "Badge" && c.kind == "function")
            .expect("Badge TSX component");
        assert!(badge_chunk.calls.contains(&"renderTag".to_string()));

        let js_src = r#"
class EventBus {
    emit(name) {
        notifyListeners(name);
    }
}
"#;
        let (_, js_chunks) = parse_file_ast("src/bus.js", js_src);
        assert!(js_chunks
            .iter()
            .any(|c| c.name == "EventBus" && c.kind == "class"));
        let emit_chunk = js_chunks
            .iter()
            .find(|c| c.name == "emit" && c.kind == "method")
            .expect("emit JS method");
        assert!(emit_chunk.calls.contains(&"notifyListeners".to_string()));

        let (_, md_chunks) = parse_file_ast("README.md", "# Title\nSome documentation text.\n");
        assert_eq!(md_chunks.len(), 1);
        assert_eq!(md_chunks[0].kind, "chunk");
    }

    #[test]
    fn sqlite_hybrid_search_and_call_graph_work() {
        let mut idx = AstIndex::open_in_memory().unwrap();
        idx.upsert_file(
            "src/auth.rs",
            100,
            r#"
pub fn verify_jwt_token(token: &str) -> bool {
    decode_claims(token)
}
pub fn decode_claims(token: &str) -> bool {
    !token.is_empty()
}
"#,
        )
        .unwrap();
        idx.upsert_file(
            "src/math.rs",
            100,
            r#"
pub fn compute_matrix_determinant(size: usize) -> f64 {
    size as f64
}
"#,
        )
        .unwrap();

        let hits = idx.search_hybrid("verify_jwt_token auth", 5).unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].rel_path, "src/auth.rs");
        assert_eq!(hits[0].name, "verify_jwt_token");
        assert!(hits[0].fts_rank.is_some());
        assert!(hits[0].vector_rank.is_some());

        let callers = idx.find_callers("decode_claims", 10).unwrap();
        assert_eq!(callers.len(), 1);
        assert_eq!(callers[0].caller_name, "verify_jwt_token");
        assert_eq!(callers[0].rel_path, "src/auth.rs");

        let outline = idx.file_outline("src/auth.rs").unwrap();
        assert_eq!(outline.len(), 2);
        assert_eq!(outline[0].name, "verify_jwt_token");
        assert_eq!(outline[1].name, "decode_claims");

        // Test custom embedding upsert and count helpers.
        let custom_vec = compute_local_embedding("custom semantic embedding");
        assert!(idx.upsert_chunk_embedding(hits[0].chunk_id, &custom_vec).unwrap());
        assert_eq!(idx.file_count().unwrap(), 2);
        assert_eq!(idx.chunk_count().unwrap(), 3);
        assert_eq!(idx.db_path(), Path::new(":memory:"));
    }

    #[test]
    fn incremental_sync_skips_unchanged_files_on_reopen() {
        let db_path = temp_db_path("incremental");
        let candidates_v1 = vec![
            SyncCandidate {
                rel_path: "src/a.rs".into(),
                mtime_ns: 1_000_000,
                abs_path: None,
                content: Some("pub fn alpha() {}".into()),
            },
            SyncCandidate {
                rel_path: "src/b.rs".into(),
                mtime_ns: 2_000_000,
                abs_path: None,
                content: Some("pub fn beta() {}".into()),
            },
        ];

        {
            let mut idx = AstIndex::open_at(&db_path).unwrap();
            let stats1 = idx.sync_incremental(&candidates_v1).unwrap();
            assert_eq!(stats1.updated, 2);
            assert_eq!(stats1.skipped, 0);
            assert_eq!(stats1.total_chunks, 2);
        }

        // Reopen persisted SQLite DB with lazy candidates (content: None, same mtime_ns).
        {
            let mut idx = AstIndex::open_at(&db_path).unwrap();
            let lazy_reopen = vec![
                SyncCandidate {
                    rel_path: "src/a.rs".into(),
                    mtime_ns: 1_000_000,
                    abs_path: None,
                    content: None,
                },
                SyncCandidate {
                    rel_path: "src/b.rs".into(),
                    mtime_ns: 2_000_000,
                    abs_path: None,
                    content: None,
                },
            ];
            let stats2 = idx.sync_incremental(&lazy_reopen).unwrap();
            assert_eq!(stats2.skipped, 2);
            assert_eq!(stats2.updated, 0);
            assert_eq!(stats2.removed, 0);
            assert_eq!(stats2.total_chunks, 2);

            // Modify one file and remove the other.
            let updated = vec![SyncCandidate {
                rel_path: "src/a.rs".into(),
                mtime_ns: 3_000_000,
                abs_path: None,
                content: Some("pub fn alpha_v2() {}".into()),
            }];
            let stats3 = idx.sync_incremental(&updated).unwrap();
            assert_eq!(stats3.updated, 1);
            assert_eq!(stats3.removed, 1);
            assert_eq!(stats3.total_chunks, 1);
        }

        let _ = fs::remove_file(&db_path);
        let _ = fs::remove_file(db_path.with_extension("db-wal"));
        let _ = fs::remove_file(db_path.with_extension("db-shm"));
    }
}
