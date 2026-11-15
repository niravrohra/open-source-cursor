use std::fs;
use std::path::Path;

const MAX_RULES_CHARS: usize = 12_000;

/// Loads workspace-level AI instruction files (`.rursorrules`, `.cursorrules`, `AGENTS.md`, `.rursor/rules/*.md`).
pub fn load_workspace_rules(root: &Path) -> Option<String> {
    let candidates = [".rursorrules", ".cursorrules", "AGENTS.md", "CLAUDE.md"];
    let mut sections = Vec::new();
    let mut total_chars = 0usize;

    for rel in candidates {
        let path = root.join(rel);
        if let Ok(content) = fs::read_to_string(&path) {
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                let take = trimmed.chars().take(MAX_RULES_CHARS - total_chars).collect::<String>();
                total_chars += take.len();
                sections.push(format!("--- Rule file: {rel} ---\n{take}"));
                if total_chars >= MAX_RULES_CHARS {
                    break;
                }
            }
        }
    }

    if total_chars < MAX_RULES_CHARS {
        let rules_dir = root.join(".rursor").join("rules");
        if let Ok(entries) = fs::read_dir(&rules_dir) {
            let mut paths: Vec<_> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("md"))
                .collect();
            paths.sort();

            for path in paths {
                if total_chars >= MAX_RULES_CHARS {
                    break;
                }
                if let Ok(content) = fs::read_to_string(&path) {
                    let trimmed = content.trim();
                    if !trimmed.is_empty() {
                        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("rule.md");
                        let take = trimmed
                            .chars()
                            .take(MAX_RULES_CHARS - total_chars)
                            .collect::<String>();
                        total_chars += take.len();
                        sections.push(format!("--- Rule file: .rursor/rules/{name} ---\n{take}"));
                    }
                }
            }
        }
    }

    if sections.is_empty() {
        None
    } else {
        Some(sections.join("\n\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::load_workspace_rules;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn loads_rursorrules_and_rules_dir() {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("rursor_rules_{nanos}"));
        fs::create_dir_all(dir.join(".rursor/rules")).unwrap();
        fs::write(dir.join(".rursorrules"), "Always write concise Rust.").unwrap();
        fs::write(dir.join(".rursor/rules/ts.md"), "Use strict TypeScript.").unwrap();

        let loaded = load_workspace_rules(&dir).unwrap();
        assert!(loaded.contains("Always write concise Rust."));
        assert!(loaded.contains("Use strict TypeScript."));

        let _ = fs::remove_dir_all(&dir);
    }
}
