use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_MEMORY_PROMPT_CHARS: usize = 6_000;

static MEMORY_ID_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryEntry {
    pub id: String,
    pub scope: String,
    pub category: String,
    pub content: String,
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SkillDefinition {
    pub name: String,
    pub description: String,
    pub source: String,
    pub body: String,
}

#[derive(Debug, Clone)]
pub struct MemoryStore {
    workspace_memory_path: Option<PathBuf>,
    global_memory_path: PathBuf,
}

impl MemoryStore {
    /// Creates a store targeting `.rursor/memory.json` (when a workspace root is available)
    /// and `~/.rursor/global-memory.json`.
    pub fn for_workspace(workspace_root: Option<&Path>) -> Self {
        let workspace_memory_path =
            workspace_root.map(|root| root.join(".rursor").join("memory.json"));
        Self {
            workspace_memory_path,
            global_memory_path: default_global_memory_path(),
        }
    }

    /// Creates a store with explicit file paths (useful for unit and integration tests).
    pub fn with_paths(workspace_memory_path: Option<PathBuf>, global_memory_path: PathBuf) -> Self {
        Self {
            workspace_memory_path,
            global_memory_path,
        }
    }

    /// Loads all persistent memories from workspace and global memory files.
    pub fn load_memories(&self) -> Vec<MemoryEntry> {
        let mut entries = Vec::new();
        if let Some(ws_path) = &self.workspace_memory_path {
            entries.extend(read_memory_file(ws_path, "workspace"));
        }
        entries.extend(read_memory_file(&self.global_memory_path, "global"));
        entries.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        entries
    }

    /// Saves a new memory entry into the requested scope (`"workspace"` or `"global"`).
    pub fn save_memory(
        &self,
        scope: &str,
        category: &str,
        content: &str,
    ) -> Result<MemoryEntry, String> {
        let trimmed_content = content.trim();
        if trimmed_content.is_empty() {
            return Err("Memory content cannot be empty.".into());
        }

        let normalized_scope = normalize_scope(scope)?;
        let normalized_category = {
            let trimmed = category.trim();
            if trimmed.is_empty() {
                "general".to_string()
            } else {
                trimmed.to_string()
            }
        };

        let target_path = match normalized_scope.as_str() {
            "workspace" => self.workspace_memory_path.as_ref().ok_or_else(|| {
                "Cannot save workspace memory without an active workspace root.".to_string()
            })?,
            "global" => &self.global_memory_path,
            _ => unreachable!(),
        };

        let mut existing = read_memory_file(target_path, &normalized_scope);
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let now_millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let seq = MEMORY_ID_SEQ.fetch_add(1, Ordering::Relaxed);

        let entry = MemoryEntry {
            id: format!("mem_{now_millis}_{seq}"),
            scope: normalized_scope,
            category: normalized_category,
            content: trimmed_content.to_string(),
            created_at: now_secs,
        };

        existing.push(entry.clone());
        write_memory_file(target_path, &existing)?;
        Ok(entry)
    }

    /// Deletes a memory entry by ID from workspace or global storage.
    /// Returns `Ok(true)` if an entry was removed, `Ok(false)` if not found.
    pub fn delete_memory(&self, id: &str) -> Result<bool, String> {
        let target_id = id.trim();
        if target_id.is_empty() {
            return Ok(false);
        }

        let mut removed = false;
        if let Some(ws_path) = &self.workspace_memory_path {
            if delete_from_file(ws_path, "workspace", target_id)? {
                removed = true;
            }
        }
        if delete_from_file(&self.global_memory_path, "global", target_id)? {
            removed = true;
        }
        Ok(removed)
    }

    /// Formats persistent memories into a system prompt block, or `None` if empty.
    pub fn format_memory_prompt(&self) -> Option<String> {
        let entries = self.load_memories();
        format_entries_for_prompt(&entries)
    }
}

/// Loads workspace and global memories for the given workspace root.
pub fn load_memories(workspace_root: Option<&Path>) -> Vec<MemoryEntry> {
    MemoryStore::for_workspace(workspace_root).load_memories()
}

/// Saves a persistent memory entry to `.rursor/memory.json` (`"workspace"`) or `~/.rursor/global-memory.json` (`"global"`).
pub fn save_memory(
    workspace_root: Option<&Path>,
    scope: &str,
    category: &str,
    content: &str,
) -> Result<MemoryEntry, String> {
    MemoryStore::for_workspace(workspace_root).save_memory(scope, category, content)
}

/// Deletes a persistent memory entry by ID from workspace or global storage.
pub fn delete_memory(workspace_root: Option<&Path>, id: &str) -> Result<bool, String> {
    MemoryStore::for_workspace(workspace_root).delete_memory(id)
}

/// Formats persistent memories into a prompt section suitable for system prompt injection.
pub fn format_memory_prompt(workspace_root: Option<&Path>) -> Option<String> {
    MemoryStore::for_workspace(workspace_root).format_memory_prompt()
}

/// Lists built-in, global (`~/.rursor/skills/*.md`), and workspace (`.rursor/skills/*.md`) skills.
/// Workspace skills override global skills, which override built-in skills of the same name.
pub fn list_skills(workspace_root: Option<&Path>) -> Vec<SkillDefinition> {
    let workspace_dir = workspace_root.map(|root| root.join(".rursor").join("skills"));
    let global_dir = default_global_skills_dir();
    list_skills_with_dirs(workspace_dir.as_deref(), Some(&global_dir))
}

/// Discovers skills using explicit directories (useful for deterministic tests).
pub fn list_skills_with_dirs(
    workspace_skills_dir: Option<&Path>,
    global_skills_dir: Option<&Path>,
) -> Vec<SkillDefinition> {
    let mut by_name: BTreeMap<String, SkillDefinition> = BTreeMap::new();

    for skill in builtin_skills() {
        by_name.insert(skill.name.to_ascii_lowercase(), skill);
    }

    if let Some(dir) = global_skills_dir {
        for skill in load_skills_from_dir(dir, "global") {
            by_name.insert(skill.name.to_ascii_lowercase(), skill);
        }
    }

    if let Some(dir) = workspace_skills_dir {
        for skill in load_skills_from_dir(dir, "workspace") {
            by_name.insert(skill.name.to_ascii_lowercase(), skill);
        }
    }

    by_name.into_values().collect()
}

/// Resolves a `/skill args` prompt into `(SkillDefinition, expanded_prompt)`.
/// Returns `None` if the prompt does not start with a known `/skill` command.
pub fn resolve_slash_command(
    workspace_root: Option<&Path>,
    prompt: &str,
) -> Option<(SkillDefinition, String)> {
    let workspace_dir = workspace_root.map(|root| root.join(".rursor").join("skills"));
    let global_dir = default_global_skills_dir();
    resolve_slash_command_with_dirs(workspace_dir.as_deref(), Some(&global_dir), prompt)
}

/// Resolves a `/skill args` prompt using explicit skill directories (for deterministic tests).
pub fn resolve_slash_command_with_dirs(
    workspace_skills_dir: Option<&Path>,
    global_skills_dir: Option<&Path>,
    prompt: &str,
) -> Option<(SkillDefinition, String)> {
    let trimmed = prompt.trim_start();
    if !trimmed.starts_with('/') {
        return None;
    }

    let without_slash = &trimmed[1..];
    let mut split_idx = without_slash.len();
    for (idx, ch) in without_slash.char_indices() {
        if ch.is_whitespace() {
            split_idx = idx;
            break;
        }
    }

    let command_name = without_slash[..split_idx].trim();
    if command_name.is_empty() || command_name.contains('/') {
        return None;
    }

    let args = without_slash[split_idx..].trim();
    let skills = list_skills_with_dirs(workspace_skills_dir, global_skills_dir);
    let skill = skills
        .into_iter()
        .find(|s| s.name.eq_ignore_ascii_case(command_name))?;

    let expanded = expand_skill_prompt(&skill, args);
    Some((skill, expanded))
}

fn expand_skill_prompt(skill: &SkillDefinition, args: &str) -> String {
    let mut body = skill.body.trim().to_string();
    let has_placeholder = body.contains("{{args}}")
        || body.contains("{args}")
        || body.contains("$ARGUMENTS");

    if has_placeholder {
        body = body
            .replace("{{args}}", args)
            .replace("{args}", args)
            .replace("$ARGUMENTS", args);
    }

    let mut out = format!(
        "Execute the `/{}` skill ({}, source: {}).\n\nInstructions:\n{}",
        skill.name, skill.description, skill.source, body
    );

    if !has_placeholder && !args.is_empty() {
        out.push_str("\n\nUser arguments / target:\n");
        out.push_str(args);
    }

    out
}

fn builtin_skills() -> Vec<SkillDefinition> {
    vec![
        SkillDefinition {
            name: "review".to_string(),
            description: "Review code changes for bugs, security risks, edge cases, and maintainability".to_string(),
            source: "builtin".to_string(),
            body: "Perform a rigorous engineering code review of the referenced files or current git diff.\n\
                   1. Check correctness, edge cases, error handling, concurrency/async safety, and security vulnerabilities.\n\
                   2. Verify API contracts, types, and test coverage.\n\
                   3. Report concrete findings ordered by severity with exact file paths, line numbers, and actionable fixes.".to_string(),
        },
        SkillDefinition {
            name: "test".to_string(),
            description: "Write or run tests covering happy paths, edge cases, and failure modes".to_string(),
            source: "builtin".to_string(),
            body: "Analyze the target module or feature and ensure comprehensive automated test coverage.\n\
                   1. Inspect existing test conventions and test runners in the workspace.\n\
                   2. Add focused unit/integration tests covering normal behavior, boundary inputs, and error paths.\n\
                   3. Run the relevant test suite and report exact pass/fail results.".to_string(),
        },
        SkillDefinition {
            name: "fix".to_string(),
            description: "Diagnose root cause of bugs, compiler errors, or failing tests and apply a minimal fix".to_string(),
            source: "builtin".to_string(),
            body: "Investigate the reported error, diagnostic, or failing test.\n\
                   1. Read the relevant code and reproduce or trace the root cause before editing.\n\
                   2. Apply a clean, minimal fix that preserves existing contracts and style.\n\
                   3. Re-run builds/tests/typechecks to verify the issue is resolved.".to_string(),
        },
        SkillDefinition {
            name: "commit".to_string(),
            description: "Inspect workspace changes and craft or execute a clean conventional commit".to_string(),
            source: "builtin".to_string(),
            body: "Review the current repository state using git status and git diff.\n\
                   1. Summarize the modified files and logical intent of the changes.\n\
                   2. Check for unintended edits, debug leftovers, or secrets.\n\
                   3. Propose a concise conventional commit message (type(scope): summary) and only run git commit if explicitly requested.".to_string(),
        },
        SkillDefinition {
            name: "explain".to_string(),
            description: "Explain code architecture, data flow, and design trade-offs clearly".to_string(),
            source: "builtin".to_string(),
            body: "Explain the requested file, symbol, or subsystem clearly and concisely.\n\
                   1. Summarize its responsibility and how it fits into the surrounding architecture.\n\
                   2. Trace the key data flow, state transitions, and invariants with exact symbol/file references.\n\
                   3. Highlight notable edge cases, assumptions, or trade-offs.".to_string(),
        },
    ]
}

fn load_skills_from_dir(dir: &Path, source: &str) -> Vec<SkillDefinition> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("md"))
        .collect();
    paths.sort();

    let mut skills = Vec::new();
    for path in paths {
        if let Ok(raw) = fs::read_to_string(&path) {
            if let Some(skill) = parse_skill_markdown(&path, &raw, source) {
                skills.push(skill);
            }
        }
    }
    skills
}

fn parse_skill_markdown(path: &Path, raw: &str, source: &str) -> Option<SkillDefinition> {
    let default_name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.trim().trim_start_matches('/').to_string())
        .filter(|s| !s.is_empty())?;

    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    let (frontmatter_name, frontmatter_desc, body) = split_frontmatter(trimmed);
    let body_trimmed = body.trim();
    if body_trimmed.is_empty() {
        return None;
    }

    let name = frontmatter_name
        .map(|n| n.trim().trim_start_matches('/').to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or(default_name);

    let description = frontmatter_desc
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| infer_description_from_body(body_trimmed, source));

    Some(SkillDefinition {
        name,
        description,
        source: source.to_string(),
        body: body_trimmed.to_string(),
    })
}

fn split_frontmatter(input: &str) -> (Option<String>, Option<String>, &str) {
    let normalized = input.strip_prefix("\u{feff}").unwrap_or(input);
    let Some(rest) = normalized.strip_prefix("---") else {
        return (None, None, normalized);
    };
    let rest = rest.strip_prefix("\r\n").or_else(|| rest.strip_prefix('\n'));
    let Some(after_open) = rest else {
        return (None, None, normalized);
    };

    let mut offset = 0usize;
    let mut name = None;
    let mut description = None;

    for line in after_open.split_inclusive('\n') {
        let line_trimmed = line.trim_end_matches(&['\r', '\n'][..]).trim();
        offset += line.len();
        if line_trimmed == "---" {
            return (name, description, &after_open[offset..]);
        }
        if let Some((key, val)) = line_trimmed.split_once(':') {
            let key = key.trim().to_ascii_lowercase();
            let val = val.trim().trim_matches('"').trim_matches('\'').to_string();
            match key.as_str() {
                "name" if !val.is_empty() => name = Some(val),
                "description" if !val.is_empty() => description = Some(val),
                _ => {}
            }
        }
    }

    (None, None, normalized)
}

fn infer_description_from_body(body: &str, source: &str) -> String {
    for line in body.lines() {
        let clean = line.trim().trim_start_matches('#').trim();
        if !clean.is_empty() {
            let mut desc: String = clean.chars().take(120).collect();
            if clean.chars().count() > 120 {
                desc.push_str("...");
            }
            return desc;
        }
    }
    format!("Custom {source} skill")
}

fn default_rursor_home() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".rursor")
}

fn default_global_memory_path() -> PathBuf {
    default_rursor_home().join("global-memory.json")
}

fn default_global_skills_dir() -> PathBuf {
    default_rursor_home().join("skills")
}

fn normalize_scope(scope: &str) -> Result<String, String> {
    match scope.trim().to_ascii_lowercase().as_str() {
        "workspace" | "project" | "local" => Ok("workspace".to_string()),
        "global" | "user" => Ok("global".to_string()),
        other => Err(format!(
            "Invalid memory scope '{other}'. Expected 'workspace' or 'global'."
        )),
    }
}

#[derive(Deserialize)]
struct WrappedMemories {
    #[serde(default)]
    entries: Vec<MemoryEntry>,
}

fn read_memory_file(path: &Path, default_scope: &str) -> Vec<MemoryEntry> {
    let Ok(raw) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    let mut entries = if let Ok(list) = serde_json::from_str::<Vec<MemoryEntry>>(trimmed) {
        list
    } else if let Ok(wrapped) = serde_json::from_str::<WrappedMemories>(trimmed) {
        wrapped.entries
    } else {
        return Vec::new();
    };

    for entry in &mut entries {
        if entry.scope.trim().is_empty() {
            entry.scope = default_scope.to_string();
        }
    }
    entries
}

fn write_memory_file(path: &Path, entries: &[MemoryEntry]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Failed to create memory dir: {e}"))?;
    }
    let json = serde_json::to_string_pretty(entries)
        .map_err(|e| format!("Failed to serialize memories: {e}"))?;
    fs::write(path, json).map_err(|e| format!("Failed to write memory file: {e}"))
}

fn delete_from_file(path: &Path, scope: &str, id: &str) -> Result<bool, String> {
    if !path.exists() {
        return Ok(false);
    }
    let mut entries = read_memory_file(path, scope);
    let orig_len = entries.len();
    entries.retain(|e| e.id != id);
    if entries.len() == orig_len {
        return Ok(false);
    }
    write_memory_file(path, &entries)?;
    Ok(true)
}

fn format_entries_for_prompt(entries: &[MemoryEntry]) -> Option<String> {
    if entries.is_empty() {
        return None;
    }

    let mut lines = Vec::new();
    let mut total_chars = 0usize;

    for entry in entries {
        let line = format!(
            "- [{} | {} | id={}] {}",
            entry.scope, entry.category, entry.id, entry.content
        );
        if total_chars + line.len() + 1 > MAX_MEMORY_PROMPT_CHARS {
            break;
        }
        total_chars += line.len() + 1;
        lines.push(line);
    }

    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_test_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = MEMORY_ID_SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("{prefix}_{nanos}_{seq}"))
    }

    #[test]
    fn memory_persistence_formatting_and_deletion() {
        let root = temp_test_dir("rursor_mem_test");
        let ws_mem = root.join("workspace/.rursor/memory.json");
        let global_mem = root.join("home/.rursor/global-memory.json");
        let store = MemoryStore::with_paths(Some(ws_mem.clone()), global_mem.clone());

        assert!(store.load_memories().is_empty());
        assert!(store.format_memory_prompt().is_none());

        let m1 = store
            .save_memory("workspace", "architecture", "Sidecar uses stdio NDJSON RPC.")
            .unwrap();
        let m2 = store
            .save_memory("global", "preference", "Prefer concise unit tests without mocks.")
            .unwrap();
        assert_ne!(m1.id, m2.id);
        assert!(ws_mem.exists());
        assert!(global_mem.exists());

        // Reload via a fresh store instance to verify persistence on disk
        let reloaded = MemoryStore::with_paths(Some(ws_mem), global_mem);
        let all = reloaded.load_memories();
        assert_eq!(all.len(), 2);

        let prompt = reloaded.format_memory_prompt().unwrap();
        assert!(prompt.contains("Sidecar uses stdio NDJSON RPC."));
        assert!(prompt.contains("Prefer concise unit tests without mocks."));
        assert!(prompt.contains("[workspace | architecture"));
        assert!(prompt.contains("[global | preference"));

        assert!(reloaded.delete_memory(&m1.id).unwrap());
        assert!(!reloaded.delete_memory("nonexistent_id").unwrap());

        let remaining = reloaded.load_memories();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, m2.id);

        assert!(reloaded.delete_memory(&m2.id).unwrap());
        assert!(reloaded.load_memories().is_empty());
        assert!(reloaded.format_memory_prompt().is_none());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_empty_memory_and_missing_workspace_scope() {
        let root = temp_test_dir("rursor_mem_err");
        let global_mem = root.join("global-memory.json");
        let store = MemoryStore::with_paths(None, global_mem);

        assert!(store.save_memory("global", "fact", "   ").is_err());
        assert!(store
            .save_memory("workspace", "fact", "Needs workspace")
            .is_err());
        assert!(store
            .save_memory("unknown_scope", "fact", "Bad scope")
            .is_err());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn discovers_builtin_global_and_workspace_skills_with_override() {
        let root = temp_test_dir("rursor_skills_test");
        let ws_skills = root.join("ws/.rursor/skills");
        let global_skills = root.join("home/.rursor/skills");
        fs::create_dir_all(&ws_skills).unwrap();
        fs::create_dir_all(&global_skills).unwrap();

        // Global custom skill + override of builtin /review
        fs::write(
            global_skills.join("deploy.md"),
            "---\ndescription: Deploy release artifacts safely\n---\nCheck CI status and tag {{args}}.",
        )
        .unwrap();
        fs::write(
            global_skills.join("review.md"),
            "# Global Review\nGlobal review instructions.",
        )
        .unwrap();

        // Workspace skill overrides global /review and adds /perf
        fs::write(
            ws_skills.join("review.md"),
            "---\nname: review\ndescription: Workspace security review\n---\nAudit Rust and TS files carefully.",
        )
        .unwrap();
        fs::write(
            ws_skills.join("perf.md"),
            "Profile hot paths and memory allocations.",
        )
        .unwrap();

        let skills = list_skills_with_dirs(Some(&ws_skills), Some(&global_skills));
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        for expected in ["commit", "deploy", "explain", "fix", "perf", "review", "test"] {
            assert!(names.contains(&expected), "missing skill {expected}");
        }

        let review = skills.iter().find(|s| s.name == "review").unwrap();
        assert_eq!(review.source, "workspace");
        assert_eq!(review.description, "Workspace security review");

        // Slash command expansion with placeholder
        let (deploy_skill, deploy_prompt) =
            resolve_slash_command_with_dirs(Some(&ws_skills), Some(&global_skills), "/deploy v1.2.0")
                .unwrap();
        assert_eq!(deploy_skill.name, "deploy");
        assert!(deploy_prompt.contains("Check CI status and tag v1.2.0."));

        // Slash command expansion without placeholder appends user args
        let (fix_skill, fix_prompt) = resolve_slash_command_with_dirs(
            Some(&ws_skills),
            Some(&global_skills),
            "/fix sidecar/src/main.rs panic on empty input",
        )
        .unwrap();
        assert_eq!(fix_skill.name, "fix");
        assert_eq!(fix_skill.source, "builtin");
        assert!(fix_prompt.contains("sidecar/src/main.rs panic on empty input"));

        // Unknown command or filesystem path returns None
        assert!(resolve_slash_command_with_dirs(
            Some(&ws_skills),
            Some(&global_skills),
            "/nonexistent foo"
        )
        .is_none());
        assert!(resolve_slash_command_with_dirs(
            Some(&ws_skills),
            Some(&global_skills),
            "/Users/niravrohra/file.rs"
        )
        .is_none());

        let _ = fs::remove_dir_all(&root);
    }
}
