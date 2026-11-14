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

#[derive(Debug, Default)]
pub struct WorkspaceIndex {
    pub root: Option<PathBuf>,
    pub files: Vec<IndexedFile>,
}

impl WorkspaceIndex {
    pub fn index_directory(&mut self, root: &Path) -> Result<usize, String> {
        if !root.is_dir() {
            return Err(format!("Workspace root is not a directory: {}", root.display()));
        }

        let ignore_names: HashSet<&'static str> = [
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
        ]
        .into_iter()
        .collect();

        let mut collected = Vec::new();
        walk_dir(root, root, &ignore_names, &mut collected);

        self.root = Some(root.to_path_buf());
        self.files = collected;
        Ok(self.files.len())
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
}
