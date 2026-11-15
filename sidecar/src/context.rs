#[path = "ast_index.rs"]
pub mod ast_index;

use ast_index::{parse_chunks, AstChunk, AstIndex, CallerHit, SyncCandidate};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_FILE_BYTES: u64 = 512 * 1024;
const MAX_INDEXED_FILES: usize = 8_000;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolInfo {
    pub name: String,
    pub kind: String,
    pub line: usize,
}

#[derive(Debug, Clone)]
pub struct IndexedFile {
    pub rel_path: String,
    pub content: String,
    pub symbols: Vec<SymbolInfo>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub path: String,
    pub score: usize,
    pub preview: String,
    pub symbols: Vec<SymbolInfo>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GrepHit {
    pub path: String,
    pub line: usize,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolSearchHit {
    pub path: String,
    pub name: String,
    pub kind: String,
    pub line: usize,
}

#[derive(Debug, Clone)]
struct GitIgnoreRule {
    base_dir: String,
    pattern: String,
    dir_only: bool,
    negated: bool,
    anchored: bool,
}

#[derive(Debug, Default)]
pub struct WorkspaceIndex {
    pub root: Option<PathBuf>,
    pub files: Vec<IndexedFile>,
    gitignore_rules: Vec<GitIgnoreRule>,
    ast_index: Option<Arc<Mutex<AstIndex>>>,
}

struct WalkedFile {
    rel_path: String,
    mtime_ns: i64,
    content: String,
}

fn ignored_dir_names() -> HashSet<&'static str> {
    [
        ".git",
        "node_modules",
        "target",
        "dist",
        "out",
        ".build",
        ".next",
        ".venv",
        "__pycache__",
        ".DS_Store",
        ".vscode-src",
        "VSCode-darwin-arm64",
    ]
    .into_iter()
    .collect()
}

fn metadata_mtime_ns(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn now_mtime_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn chunks_to_symbol_infos(chunks: &[AstChunk]) -> Vec<SymbolInfo> {
    chunks
        .iter()
        .filter(|c| c.kind != "chunk" && c.kind != "module")
        .take(120)
        .map(|c| SymbolInfo {
            name: c.name.clone(),
            kind: c.kind.clone(),
            line: c.start_line,
        })
        .collect()
}

impl WorkspaceIndex {
    pub fn index_directory(&mut self, root: &Path) -> Result<usize, String> {
        if !root.is_dir() {
            return Err(format!("Workspace root is not a directory: {}", root.display()));
        }
        let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let ast = AstIndex::open_for_workspace(&canonical_root)
            .or_else(|_| AstIndex::open_in_memory())?;
        self.index_directory_inner(&canonical_root, ast)
    }

    pub fn index_directory_with_db(&mut self, root: &Path, db_path: &Path) -> Result<usize, String> {
        if !root.is_dir() {
            return Err(format!("Workspace root is not a directory: {}", root.display()));
        }
        let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let ast = AstIndex::open_at(db_path)?;
        self.index_directory_inner(&canonical_root, ast)
    }

    fn index_directory_inner(
        &mut self,
        canonical_root: &Path,
        mut ast: AstIndex,
    ) -> Result<usize, String> {
        let ignore_names = ignored_dir_names();
        let mut rules = Vec::new();
        let mut walked = Vec::new();
        walk_dir(
            canonical_root,
            canonical_root,
            &ignore_names,
            &mut rules,
            &mut walked,
        );

        let candidates: Vec<SyncCandidate> = walked
            .iter()
            .map(|f| SyncCandidate {
                rel_path: f.rel_path.clone(),
                mtime_ns: f.mtime_ns,
                abs_path: None,
                content: Some(f.content.clone()),
            })
            .collect();

        ast.sync_incremental(&candidates)?;
        let symbol_map = ast.load_all_symbols().unwrap_or_default();

        let collected = walked
            .into_iter()
            .map(|f| {
                let symbols = symbol_map
                    .get(&f.rel_path)
                    .map(|entries| {
                        entries
                            .iter()
                            .take(120)
                            .map(|(name, kind, line)| SymbolInfo {
                                name: name.clone(),
                                kind: kind.clone(),
                                line: *line,
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                IndexedFile {
                    rel_path: f.rel_path,
                    content: f.content,
                    symbols,
                }
            })
            .collect();

        self.root = Some(canonical_root.to_path_buf());
        self.gitignore_rules = rules;
        self.files = collected;
        self.ast_index = Some(Arc::new(Mutex::new(ast)));
        Ok(self.files.len())
    }

    pub fn upsert_file(&mut self, path: &Path, content_override: Option<String>) -> Result<bool, String> {
        let Some(root) = &self.root else {
            return Err("Workspace is not indexed yet.".into());
        };

        let rel_path = self.relative_path(root, path)?;
        if is_ignored_rel_path(&rel_path) || is_gitignored(&rel_path, false, &self.gitignore_rules) {
            return Ok(false);
        }

        let (content, mtime_ns) = match content_override {
            Some(c) => {
                if c.len() as u64 > MAX_FILE_BYTES || c.as_bytes().contains(&0) {
                    return Ok(false);
                }
                let disk_mtime = fs::metadata(root.join(&rel_path))
                    .ok()
                    .map(|m| metadata_mtime_ns(&m))
                    .unwrap_or_else(now_mtime_ns);
                (c, disk_mtime)
            }
            None => {
                let full_path = root.join(&rel_path);
                let meta = fs::metadata(&full_path)
                    .map_err(|e| format!("Failed to stat {}: {e}", rel_path))?;
                if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
                    return Ok(false);
                }
                let bytes = fs::read(&full_path)
                    .map_err(|e| format!("Failed to read {}: {e}", rel_path))?;
                if bytes.contains(&0) {
                    return Ok(false);
                }
                let text = String::from_utf8(bytes)
                    .map_err(|_| format!("File is not valid UTF-8: {rel_path}"))?;
                (text, metadata_mtime_ns(&meta))
            }
        };

        let chunks = parse_chunks(&rel_path, &content);
        let symbols = chunks_to_symbol_infos(&chunks);

        if let Some(ast) = &self.ast_index {
            if let Ok(mut guard) = ast.lock() {
                let _ = guard.upsert_file(&rel_path, mtime_ns, &content);
            }
        }

        if let Some(existing) = self.files.iter_mut().find(|f| f.rel_path == rel_path) {
            existing.content = content;
            existing.symbols = symbols;
        } else if self.files.len() < MAX_INDEXED_FILES {
            self.files.push(IndexedFile {
                rel_path,
                content,
                symbols,
            });
        }
        Ok(true)
    }

    pub fn remove_file(&mut self, path: &Path) -> bool {
        let Some(root) = &self.root else {
            return false;
        };
        let Ok(rel_path) = self.relative_path(root, path) else {
            return false;
        };
        let before = self.files.len();
        self.files.retain(|f| f.rel_path != rel_path);
        if let Some(ast) = &self.ast_index {
            if let Ok(mut guard) = ast.lock() {
                let _ = guard.remove_file(&rel_path);
            }
        }
        self.files.len() != before
    }

    pub fn list_files(&self, prefix: Option<&str>, limit: usize) -> Vec<String> {
        let normalized_prefix = prefix
            .map(|p| p.trim().trim_start_matches("./").trim_end_matches('/').to_string())
            .filter(|p| !p.is_empty());

        let mut paths: Vec<String> = self
            .files
            .iter()
            .filter(|f| match &normalized_prefix {
                Some(pref) => f.rel_path == *pref || f.rel_path.starts_with(&format!("{pref}/")),
                None => true,
            })
            .map(|f| f.rel_path.clone())
            .collect();
        paths.sort();
        paths.truncate(limit);
        paths
    }

    pub fn read_file(
        &self,
        requested_path: &str,
        start_line: Option<usize>,
        end_line: Option<usize>,
    ) -> Result<String, String> {
        let Some(root) = &self.root else {
            return Err("No workspace root is open.".into());
        };

        let rel = self.resolve_confined_path(root, requested_path)?;
        let content = fs::read_to_string(&rel.abs_path)
            .map_err(|e| format!("Cannot read '{}': {e}", rel.rel_path))?;

        let lines: Vec<&str> = content.lines().collect();
        if lines.is_empty() {
            return Ok(String::new());
        }

        let start = start_line.unwrap_or(1).max(1);
        let end = end_line.unwrap_or(lines.len().min(start + 249)).min(lines.len());
        if start > end {
            return Ok(String::new());
        }

        let mut out = String::new();
        for idx in (start - 1)..end {
            out.push_str(&format!("{}: {}\n", idx + 1, lines[idx]));
        }
        Ok(out)
    }

    /// Hybrid search using SQLite FTS5 BM25 + 128-dim vector cosine similarity with RRF when
    /// backed by `AstIndex`, falling back to in-memory BM25 if not yet persisted.
    pub fn search(&self, query: &str, limit: usize) -> Vec<SearchHit> {
        if limit == 0 || query.trim().is_empty() {
            return Vec::new();
        }

        if let Some(ast) = &self.ast_index {
            if let Ok(guard) = ast.lock() {
                if let Ok(chunk_hits) = guard.search_hybrid(query, (limit * 4).max(16)) {
                    if !chunk_hits.is_empty() {
                        let file_map: HashMap<&str, &IndexedFile> = self
                            .files
                            .iter()
                            .map(|f| (f.rel_path.as_str(), f))
                            .collect();
                        let mut seen_files = HashSet::new();
                        let mut out = Vec::new();

                        for hit in chunk_hits {
                            if !seen_files.insert(hit.rel_path.clone()) {
                                continue;
                            }
                            let symbols = file_map
                                .get(hit.rel_path.as_str())
                                .map(|f| f.symbols.iter().take(12).cloned().collect())
                                .unwrap_or_else(|| {
                                    vec![SymbolInfo {
                                        name: hit.name.clone(),
                                        kind: hit.kind.clone(),
                                        line: hit.start_line,
                                    }]
                                });
                            let preview_lines: Vec<&str> = hit.content.lines().take(20).collect();
                            let preview = if preview_lines.is_empty() {
                                hit.signature.clone()
                            } else {
                                preview_lines.join("\n")
                            };
                            let score = (hit.rrf_score * 10_000.0).round().max(1.0) as usize;
                            out.push(SearchHit {
                                path: hit.rel_path,
                                score,
                                preview,
                                symbols,
                            });
                            if out.len() >= limit {
                                break;
                            }
                        }

                        if !out.is_empty() {
                            return out;
                        }
                    }
                }
            }
        }

        self.in_memory_bm25_search(query, limit)
    }

    fn in_memory_bm25_search(&self, query: &str, limit: usize) -> Vec<SearchHit> {
        let terms: Vec<String> = query
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|t| t.len() >= 2)
            .map(|t| t.to_lowercase())
            .collect();

        if terms.is_empty() || self.files.is_empty() {
            return Vec::new();
        }

        let total_docs = self.files.len() as f64;
        let avg_len = (self
            .files
            .iter()
            .map(|f| f.content.len().max(1))
            .sum::<usize>() as f64)
            / total_docs;

        let dfs: Vec<usize> = terms
            .iter()
            .map(|term| {
                self.files
                    .iter()
                    .filter(|f| {
                        f.rel_path.to_lowercase().contains(term)
                            || f.content.to_lowercase().contains(term)
                    })
                    .count()
            })
            .collect();

        let mut scored: Vec<SearchHit> = self
            .files
            .iter()
            .filter_map(|file| {
                let path_lower = file.rel_path.to_lowercase();
                let content_lower = file.content.to_lowercase();
                let doc_len_ratio = (file.content.len().max(1) as f64) / avg_len.max(1.0);
                let mut score_f = 0.0f64;

                for (idx, term) in terms.iter().enumerate() {
                    let df = dfs[idx] as f64;
                    let idf = ((total_docs - df + 0.5) / (df + 0.5) + 1.0).ln().max(0.5);

                    if path_lower.contains(term) {
                        score_f += 16.0 * idf;
                    }

                    let symbol_match = file.symbols.iter().any(|s| s.name.to_lowercase().contains(term));
                    if symbol_match {
                        score_f += 14.0 * idf;
                    }

                    let tf = content_lower.matches(term).count().min(30) as f64;
                    if tf > 0.0 {
                        let k1 = 1.2;
                        let b = 0.5;
                        let bm25_tf = (tf * (k1 + 1.0)) / (tf + k1 * (1.0 - b + b * doc_len_ratio));
                        score_f += bm25_tf * 4.0 * idf;
                    }
                }

                let score = score_f.round() as usize;
                if score == 0 {
                    return None;
                }

                let preview = extract_preview(&file.content, &terms);
                Some(SearchHit {
                    path: file.rel_path.clone(),
                    score,
                    preview,
                    symbols: file.symbols.iter().take(12).cloned().collect(),
                })
            })
            .collect();

        scored.sort_by(|a, b| b.score.cmp(&a.score));
        scored.truncate(limit);
        scored
    }

    pub fn find_callers(&self, symbol: &str, limit: usize) -> Vec<CallerHit> {
        if let Some(ast) = &self.ast_index {
            if let Ok(guard) = ast.lock() {
                if let Ok(hits) = guard.find_callers(symbol, limit) {
                    return hits;
                }
            }
        }

        let needle = symbol.trim();
        if needle.is_empty() || limit == 0 {
            return Vec::new();
        }
        let mut hits = Vec::new();
        for file in &self.files {
            for chunk in parse_chunks(&file.rel_path, &file.content) {
                let matched = chunk.calls.iter().any(|call| {
                    call == needle
                        || call.rsplit("::").next() == Some(needle)
                        || call.rsplit('.').next() == Some(needle)
                });
                if matched {
                    hits.push(CallerHit {
                        rel_path: file.rel_path.clone(),
                        caller_name: chunk.name,
                        caller_kind: chunk.kind,
                        start_line: chunk.start_line,
                        end_line: chunk.end_line,
                        signature: chunk.signature,
                    });
                    if hits.len() >= limit {
                        return hits;
                    }
                }
            }
        }
        hits
    }

    pub fn file_outline(&self, rel_path: &str) -> Vec<AstChunk> {
        let normalized = rel_path
            .trim()
            .trim_start_matches("./")
            .replace('\\', "/");

        if let Some(ast) = &self.ast_index {
            if let Ok(guard) = ast.lock() {
                if let Ok(outline) = guard.file_outline(&normalized) {
                    if !outline.is_empty() {
                        return outline;
                    }
                }
            }
        }

        if let Some(file) = self.files.iter().find(|f| f.rel_path == normalized) {
            return parse_chunks(&file.rel_path, &file.content);
        }
        Vec::new()
    }

    pub fn search_symbols(&self, query: &str, limit: usize) -> Vec<SymbolSearchHit> {
        let needle = query.trim().to_lowercase();
        let mut out = Vec::new();
        for file in &self.files {
            for sym in &file.symbols {
                if needle.is_empty() || sym.name.to_lowercase().contains(&needle) {
                    out.push(SymbolSearchHit {
                        path: file.rel_path.clone(),
                        name: sym.name.clone(),
                        kind: sym.kind.clone(),
                        line: sym.line,
                    });
                    if out.len() >= limit {
                        return out;
                    }
                }
            }
        }
        out
    }

    pub fn grep(&self, query: &str, case_sensitive: bool, limit: usize) -> Vec<GrepHit> {
        let needle = query.trim();
        if needle.is_empty() {
            return Vec::new();
        }
        let needle_lower = needle.to_lowercase();
        let mut hits = Vec::new();

        for file in &self.files {
            for (idx, line) in file.content.lines().enumerate() {
                let matched = if case_sensitive {
                    line.contains(needle)
                } else {
                    line.to_lowercase().contains(&needle_lower)
                };
                if matched {
                    hits.push(GrepHit {
                        path: file.rel_path.clone(),
                        line: idx + 1,
                        text: line.chars().take(240).collect(),
                    });
                    if hits.len() >= limit {
                        return hits;
                    }
                }
            }
        }
        hits
    }

    pub fn git_context(&self) -> Result<String, String> {
        let Some(root) = &self.root else {
            return Err("No workspace root is open.".into());
        };
        let status_out = Command::new("git")
            .args(["status", "--short"])
            .current_dir(root)
            .output()
            .map_err(|e| format!("Failed to run git status: {e}"))?;
        let diff_out = Command::new("git")
            .args(["diff", "--unified=3"])
            .current_dir(root)
            .output()
            .map_err(|e| format!("Failed to run git diff: {e}"))?;

        let status = String::from_utf8_lossy(&status_out.stdout);
        let diff = String::from_utf8_lossy(&diff_out.stdout);
        let truncated_diff: String = diff.chars().take(12_000).collect();

        Ok(format!(
            "--- git status --short ---\n{}\n--- git diff ---\n{}",
            if status.trim().is_empty() {
                "(clean)"
            } else {
                status.trim()
            },
            if truncated_diff.trim().is_empty() {
                "(no unstaged diff)"
            } else {
                truncated_diff.trim()
            }
        ))
    }

    fn relative_path(&self, root: &Path, path: &Path) -> Result<String, String> {
        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            root.join(path)
        };
        let normalized = fs::canonicalize(&candidate).unwrap_or(candidate);
        let rel = normalized
            .strip_prefix(root)
            .map_err(|_| format!("Path '{}' is outside workspace root", path.display()))?;
        Ok(rel.to_string_lossy().replace('\\', "/"))
    }

    fn resolve_confined_path(&self, root: &Path, requested: &str) -> Result<ResolvedPath, String> {
        let raw = Path::new(requested);
        let candidate = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            root.join(raw)
        };
        let canonical = fs::canonicalize(&candidate)
            .map_err(|e| format!("Path '{}' does not exist or cannot be resolved: {e}", requested))?;
        let rel = canonical
            .strip_prefix(root)
            .map_err(|_| format!("Access denied: '{}' is outside the workspace", requested))?;
        Ok(ResolvedPath {
            abs_path: canonical.clone(),
            rel_path: rel.to_string_lossy().replace('\\', "/"),
        })
    }
}

#[allow(dead_code)]
pub fn format_context_block(hits: &[SearchHit]) -> String {
    let mut out = String::new();
    for hit in hits {
        out.push_str(&format!(
            "\n--- Workspace Match: {} (score {}) ---\n{}\n",
            hit.path, hit.score, hit.preview
        ));
    }
    out
}

struct ResolvedPath {
    abs_path: PathBuf,
    rel_path: String,
}

fn is_ignored_rel_path(rel_path: &str) -> bool {
    let ignored = ignored_dir_names();
    Path::new(rel_path).components().any(|comp| {
        comp.as_os_str()
            .to_str()
            .map(|s| ignored.contains(s))
            .unwrap_or(false)
    })
}

fn load_gitignore_rules(root: &Path, dir: &Path) -> Vec<GitIgnoreRule> {
    let gi_path = dir.join(".gitignore");
    let Ok(content) = fs::read_to_string(&gi_path) else {
        return Vec::new();
    };
    let base_dir = dir
        .strip_prefix(root)
        .unwrap_or(Path::new(""))
        .to_string_lossy()
        .replace('\\', "/");

    let mut rules = Vec::new();
    for raw_line in content.lines() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let (negated, rest) = if let Some(stripped) = trimmed.strip_prefix('!') {
            (true, stripped)
        } else {
            (false, trimmed)
        };
        let dir_only = rest.ends_with('/');
        let no_trail = rest.trim_end_matches('/');
        let anchored = no_trail.starts_with('/');
        let pattern = no_trail.trim_start_matches('/').to_string();
        if pattern.is_empty() {
            continue;
        }
        rules.push(GitIgnoreRule {
            base_dir: base_dir.clone(),
            pattern,
            dir_only,
            negated,
            anchored,
        });
    }
    rules
}

fn is_gitignored(rel_path: &str, is_dir: bool, rules: &[GitIgnoreRule]) -> bool {
    let mut ignored = false;
    for rule in rules {
        if rule.dir_only && !is_dir {
            continue;
        }
        let sub_path = if rule.base_dir.is_empty() {
            rel_path
        } else if let Some(rest) = rel_path.strip_prefix(&format!("{}/", rule.base_dir)) {
            rest
        } else {
            continue;
        };

        let matched = if rule.anchored || rule.pattern.contains('/') {
            glob_match(&rule.pattern, sub_path)
        } else {
            let file_name = sub_path.rsplit('/').next().unwrap_or(sub_path);
            glob_match(&rule.pattern, file_name) || glob_match(&rule.pattern, sub_path)
        };

        if matched {
            ignored = !rule.negated;
        }
    }
    ignored
}

fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    glob_match_inner(&p, &t)
}

fn glob_match_inner(p: &[char], t: &[char]) -> bool {
    match (p.first(), t.first()) {
        (None, None) => true,
        (Some('*'), _) => {
            let mut rest_p = &p[1..];
            while rest_p.first() == Some(&'*') {
                rest_p = &rest_p[1..];
            }
            if glob_match_inner(rest_p, t) {
                return true;
            }
            !t.is_empty() && glob_match_inner(p, &t[1..])
        }
        (Some('?'), Some(_)) => glob_match_inner(&p[1..], &t[1..]),
        (Some(pc), Some(tc)) if pc == tc => glob_match_inner(&p[1..], &t[1..]),
        _ => false,
    }
}

pub fn extract_symbols(content: &str) -> Vec<SymbolInfo> {
    let mut symbols = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();
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

        let extracted: Option<(&str, &str)> = if let Some(rest) = clean.strip_prefix("fn ") {
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

        if let Some((kind, rest)) = extracted {
            let ident: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if ident.len() >= 2 {
                symbols.push(SymbolInfo {
                    name: ident,
                    kind: kind.to_string(),
                    line: idx + 1,
                });
                if symbols.len() >= 120 {
                    break;
                }
            }
        }
    }
    symbols
}

fn walk_dir(
    root: &Path,
    dir: &Path,
    ignore_names: &HashSet<&str>,
    rules: &mut Vec<GitIgnoreRule>,
    out: &mut Vec<WalkedFile>,
) {
    if out.len() >= MAX_INDEXED_FILES {
        return;
    }

    let prev_rule_len = rules.len();
    rules.extend(load_gitignore_rules(root, dir));

    let Ok(entries) = fs::read_dir(dir) else {
        rules.truncate(prev_rule_len);
        return;
    };

    for entry in entries.flatten() {
        if out.len() >= MAX_INDEXED_FILES {
            break;
        }

        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if ignore_names.contains(name) {
            continue;
        }

        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let rel_path = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");

        if is_gitignored(&rel_path, meta.is_dir(), rules) {
            continue;
        }

        if meta.is_dir() {
            walk_dir(root, &path, ignore_names, rules, out);
        } else if meta.is_file() && meta.len() <= MAX_FILE_BYTES {
            let Ok(bytes) = fs::read(&path) else {
                continue;
            };
            if bytes.contains(&0) {
                continue;
            }
            let Ok(content) = String::from_utf8(bytes) else {
                continue;
            };
            out.push(WalkedFile {
                rel_path,
                mtime_ns: metadata_mtime_ns(&meta),
                content,
            });
        }
    }

    rules.truncate(prev_rule_len);
}

fn extract_preview(content: &str, terms: &[String]) -> String {
    let lines: Vec<&str> = content.lines().collect();
    for (idx, line) in lines.iter().enumerate() {
        let lower = line.to_lowercase();
        if terms.iter().any(|t| lower.contains(t)) {
            let start = idx.saturating_sub(3);
            let end = (idx + 12).min(lines.len());
            return lines[start..end].join("\n");
        }
    }
    lines.iter().take(15).copied().collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("rursor_test_{name}_{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn search_ranks_matching_files_and_symbols() {
        let mut index = WorkspaceIndex::default();
        let auth_src = "pub fn verify_token(token: &str) -> bool { true }";
        let math_src = "pub fn add(a: i32, b: i32) -> i32 { a + b }";
        index.files = vec![
            IndexedFile {
                rel_path: "src/auth.rs".into(),
                content: auth_src.into(),
                symbols: extract_symbols(auth_src),
            },
            IndexedFile {
                rel_path: "src/math.rs".into(),
                content: math_src.into(),
                symbols: extract_symbols(math_src),
            },
        ];

        let hits = index.search("verify_token auth", 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "src/auth.rs");

        let syms = index.search_symbols("verify", 5);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "verify_token");
        assert_eq!(syms[0].kind, "fn");
    }

    #[test]
    fn respects_gitignore_rules() {
        let root = make_temp_dir("gitignore");
        let db_path = root.join(".test_index.db");
        fs::write(root.join(".gitignore"), "*.log\nignored_dir/\n!keep.log\n.test_index.db*\n").unwrap();
        fs::create_dir_all(root.join("ignored_dir")).unwrap();
        fs::write(root.join("ignored_dir/secret.txt"), "hidden").unwrap();
        fs::write(root.join("debug.log"), "skip me").unwrap();
        fs::write(root.join("keep.log"), "keep me").unwrap();
        fs::write(root.join("main.rs"), "fn main() {}").unwrap();

        let mut index = WorkspaceIndex::default();
        index.index_directory_with_db(&root, &db_path).unwrap();
        let paths = index.list_files(None, 20);
        assert!(paths.contains(&"main.rs".to_string()));
        assert!(paths.contains(&"keep.log".to_string()));
        assert!(!paths.contains(&"debug.log".to_string()));
        assert!(!paths.contains(&"ignored_dir/secret.txt".to_string()));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn read_file_blocks_path_traversal_and_upserts() {
        let root = make_temp_dir("read_confine");
        let db_path = make_temp_dir("read_confine_db").join("index.db");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/hello.txt"), "line1\nline2\nline3\n").unwrap();

        let mut index = WorkspaceIndex::default();
        assert_eq!(index.index_directory_with_db(&root, &db_path).unwrap(), 1);

        let content = index.read_file("src/hello.txt", Some(2), Some(3)).unwrap();
        assert_eq!(content, "2: line2\n3: line3\n");

        assert!(index.read_file("../etc/passwd", None, None).is_err());

        index
            .upsert_file(Path::new("src/new.txt"), Some("pub struct Demo {}".into()))
            .unwrap();
        assert_eq!(index.files.len(), 2);
        assert!(index.remove_file(Path::new("src/new.txt")));
        assert_eq!(index.files.len(), 1);

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(db_path.parent().unwrap());
    }

    #[test]
    fn workspace_index_uses_ast_hybrid_search_callers_and_outline() {
        let root = make_temp_dir("ast_workspace");
        let db_path = make_temp_dir("ast_workspace_db").join("index.db");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/auth.rs"),
            r#"
pub fn login(user: &str) -> bool {
    verify_session_token(user)
}

pub fn verify_session_token(token: &str) -> bool {
    !token.is_empty()
}
"#,
        )
        .unwrap();
        fs::write(
            root.join("src/routes.ts"),
            r#"
export function handleAuthRequest(req: string): boolean {
    return validateHeader(req);
}
"#,
        )
        .unwrap();

        let mut index = WorkspaceIndex::default();
        assert_eq!(index.index_directory_with_db(&root, &db_path).unwrap(), 2);

        let hits = index.search("verify_session_token", 5);
        assert!(!hits.is_empty());
        assert_eq!(hits[0].path, "src/auth.rs");

        let callers = index.find_callers("verify_session_token", 5);
        assert_eq!(callers.len(), 1);
        assert_eq!(callers[0].caller_name, "login");

        let outline = index.file_outline("src/auth.rs");
        assert_eq!(outline.len(), 2);
        assert_eq!(outline[0].name, "login");
        assert_eq!(outline[1].name, "verify_session_token");

        let ts_syms = index.search_symbols("handleAuthRequest", 5);
        assert_eq!(ts_syms.len(), 1);
        assert_eq!(ts_syms[0].kind, "function");

        let block = format_context_block(&hits);
        assert!(block.contains("src/auth.rs"));

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(db_path.parent().unwrap());
    }
}
