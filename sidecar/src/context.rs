use serde::Serialize;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

const MAX_FILE_BYTES: u64 = 512 * 1024;
const MAX_INDEXED_FILES: usize = 5_000;

#[derive(Debug, Clone)]
pub struct IndexedFile {
    pub rel_path: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub path: String,
    pub score: usize,
    pub preview: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GrepHit {
    pub path: String,
    pub line: usize,
    pub text: String,
}

#[derive(Debug, Default)]
pub struct WorkspaceIndex {
    pub root: Option<PathBuf>,
    pub files: Vec<IndexedFile>,
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
    ]
    .into_iter()
    .collect()
}

impl WorkspaceIndex {
    pub fn index_directory(&mut self, root: &Path) -> Result<usize, String> {
        if !root.is_dir() {
            return Err(format!("Workspace root is not a directory: {}", root.display()));
        }

        let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let ignore_names = ignored_dir_names();

        let mut collected = Vec::new();
        walk_dir(&canonical_root, &canonical_root, &ignore_names, &mut collected);

        self.root = Some(canonical_root);
        self.files = collected;
        Ok(self.files.len())
    }

    pub fn upsert_file(&mut self, path: &Path, content_override: Option<String>) -> Result<bool, String> {
        let Some(root) = &self.root else {
            return Err("Workspace is not indexed yet.".into());
        };

        let rel_path = self.relative_path(root, path)?;
        if is_ignored_rel_path(&rel_path) {
            return Ok(false);
        }

        let content = match content_override {
            Some(c) => {
                if c.len() as u64 > MAX_FILE_BYTES || c.as_bytes().contains(&0) {
                    return Ok(false);
                }
                c
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
                String::from_utf8(bytes)
                    .map_err(|_| format!("File is not valid UTF-8: {rel_path}"))?
            }
        };

        if let Some(existing) = self.files.iter_mut().find(|f| f.rel_path == rel_path) {
            existing.content = content;
        } else if self.files.len() < MAX_INDEXED_FILES {
            self.files.push(IndexedFile { rel_path, content });
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

    /// Scores files by keyword occurrences across path and contents.
    pub fn search(&self, query: &str, limit: usize) -> Vec<SearchHit> {
        let terms: Vec<String> = query
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|t| t.len() >= 2)
            .map(|t| t.to_lowercase())
            .collect();

        if terms.is_empty() {
            return Vec::new();
        }

        let mut scored: Vec<SearchHit> = self
            .files
            .iter()
            .filter_map(|file| {
                let path_lower = file.rel_path.to_lowercase();
                let content_lower = file.content.to_lowercase();
                let mut score = 0usize;

                for term in &terms {
                    if path_lower.contains(term) {
                        score += 10;
                    }
                    let hits = content_lower.matches(term).count();
                    score += hits.min(20);
                }

                if score == 0 {
                    return None;
                }

                let preview = extract_preview(&file.content, &terms);
                Some(SearchHit {
                    path: file.rel_path.clone(),
                    score,
                    preview,
                })
            })
            .collect();

        scored.sort_by(|a, b| b.score.cmp(&a.score));
        scored.truncate(limit);
        scored
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
        Ok(rel.to_string_lossy().to_string())
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
            rel_path: rel.to_string_lossy().to_string(),
        })
    }
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

fn walk_dir(
    root: &Path,
    dir: &Path,
    ignore_names: &HashSet<&str>,
    out: &mut Vec<IndexedFile>,
) {
    if out.len() >= MAX_INDEXED_FILES {
        return;
    }

    let Ok(entries) = fs::read_dir(dir) else {
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

        if meta.is_dir() {
            walk_dir(root, &path, ignore_names, out);
        } else if meta.is_file() && meta.len() <= MAX_FILE_BYTES {
            let Ok(bytes) = fs::read(&path) else {
                continue;
            };
            // Skip binary files containing null bytes.
            if bytes.contains(&0) {
                continue;
            }
            let Ok(content) = String::from_utf8(bytes) else {
                continue;
            };
            let rel_path = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            out.push(IndexedFile { rel_path, content });
        }
    }
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
    use std::time::{SystemTime, UNIX_EPOCH};

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
    fn search_ranks_matching_files() {
        let mut index = WorkspaceIndex::default();
        index.files = vec![
            IndexedFile {
                rel_path: "src/auth.rs".into(),
                content: "pub fn verify_token(token: &str) -> bool { true }".into(),
            },
            IndexedFile {
                rel_path: "src/math.rs".into(),
                content: "pub fn add(a: i32, b: i32) -> i32 { a + b }".into(),
            },
        ];

        let hits = index.search("verify_token auth", 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "src/auth.rs");
    }

    #[test]
    fn grep_and_list_files_work() {
        let mut index = WorkspaceIndex::default();
        index.files = vec![
            IndexedFile {
                rel_path: "src/a.rs".into(),
                content: "fn alpha() {}\nfn beta() {}".into(),
            },
            IndexedFile {
                rel_path: "tests/t.rs".into(),
                content: "assert_eq!(1, 1);".into(),
            },
        ];

        assert_eq!(index.list_files(Some("src"), 10), vec!["src/a.rs"]);
        let hits = index.grep("BETA", false, 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line, 2);
        assert!(index.grep("BETA", true, 10).is_empty());
    }

    #[test]
    fn read_file_blocks_path_traversal_and_upserts() {
        let root = make_temp_dir("read_confine");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/hello.txt"), "line1\nline2\nline3\n").unwrap();

        let mut index = WorkspaceIndex::default();
        assert_eq!(index.index_directory(&root).unwrap(), 1);

        let content = index.read_file("src/hello.txt", Some(2), Some(3)).unwrap();
        assert_eq!(content, "2: line2\n3: line3\n");

        let traversal = index.read_file("../etc/passwd", None, None);
        assert!(traversal.is_err());

        index
            .upsert_file(Path::new("src/new.txt"), Some("hello world".into()))
            .unwrap();
        assert_eq!(index.files.len(), 2);
        assert!(index.remove_file(Path::new("src/new.txt")));
        assert_eq!(index.files.len(), 1);

        let _ = fs::remove_dir_all(&root);
    }
}
