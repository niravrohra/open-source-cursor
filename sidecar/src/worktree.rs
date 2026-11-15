use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_CHECKPOINT_SEQ: AtomicU64 = AtomicU64::new(1);
static NEXT_WORKTREE_SEQ: AtomicU64 = AtomicU64::new(1);

const MAX_FULL_SNAPSHOT_FILES: usize = 2000;
const IGNORED_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".next",
    "dist",
    "build",
    ".venv",
    "__pycache__",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointSummary {
    pub checkpoint_id: String,
    pub label: String,
    pub created_at: u64,
    pub file_count: usize,
    pub git_status: Option<String>,
    pub git_head: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RestoreResult {
    pub checkpoint_id: String,
    pub restored_files: Vec<String>,
    pub removed_files: Vec<String>,
    pub git_status_before: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckpointFileEntry {
    rel_path: String,
    existed: bool,
    snapshot_rel: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckpointManifest {
    checkpoint_id: String,
    label: String,
    created_at: u64,
    workspace_root: String,
    full_workspace: bool,
    snapshotted_dirs: Vec<String>,
    git_status: Option<String>,
    git_head: Option<String>,
    files: Vec<CheckpointFileEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeInfo {
    pub path: String,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub is_main: bool,
    pub is_bare: bool,
}

/// Manages real file-level checkpoints under `~/.rursor/checkpoints/<workspace_hash>/<checkpoint_id>/`.
pub struct CheckpointStore {
    base_dir: PathBuf,
}

impl CheckpointStore {
    pub fn default_store() -> Self {
        Self {
            base_dir: rursor_home_dir().join("checkpoints"),
        }
    }

    pub fn with_dir(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    fn workspace_dir(&self, workspace_root: &Path) -> PathBuf {
        self.base_dir.join(workspace_hash(workspace_root))
    }

    fn checkpoint_dir(&self, workspace_root: &Path, checkpoint_id: &str) -> Result<PathBuf, String> {
        let clean_id = sanitize_id(checkpoint_id);
        if clean_id.is_empty() || clean_id != checkpoint_id {
            return Err(format!("Invalid checkpoint ID: '{checkpoint_id}'"));
        }
        Ok(self.workspace_dir(workspace_root).join(clean_id))
    }

    /// Creates a new checkpoint capturing the given `target_paths` (or the entire workspace if
    /// `target_paths` is empty), along with git status/HEAD metadata when inside a git repo.
    pub fn create_checkpoint(
        &self,
        workspace_root: &Path,
        label: &str,
        target_paths: &[PathBuf],
    ) -> Result<CheckpointSummary, String> {
        if !workspace_root.exists() {
            return Err(format!(
                "Workspace root does not exist: {}",
                workspace_root.display()
            ));
        }

        let canonical_root = workspace_root
            .canonicalize()
            .unwrap_or_else(|_| workspace_root.to_path_buf());

        let now_secs = epoch_secs();
        let now_millis = epoch_millis();
        let seq = NEXT_CHECKPOINT_SEQ.fetch_add(1, Ordering::Relaxed);
        let checkpoint_id = format!("cp_{now_millis}_{seq}");

        let cp_dir = self.workspace_dir(&canonical_root).join(&checkpoint_id);
        let files_dir = cp_dir.join("files");
        fs::create_dir_all(&files_dir)
            .map_err(|e| format!("Failed to create checkpoint directory: {e}"))?;

        let full_workspace = target_paths.is_empty();
        let mut snapshotted_dirs = Vec::new();
        let mut collected_rel_paths: Vec<String> = Vec::new();
        let mut seen = HashSet::new();

        if full_workspace {
            collect_dir_files(
                &canonical_root,
                &canonical_root,
                &mut collected_rel_paths,
                &mut seen,
                MAX_FULL_SNAPSHOT_FILES,
            )?;
        } else {
            for target in target_paths {
                let abs = resolve_target_path(&canonical_root, target)?;
                let rel = abs
                    .strip_prefix(&canonical_root)
                    .map_err(|_| format!("Path escapes workspace: {}", target.display()))?;
                let rel_str = normalize_rel_path(rel);
                if rel_str.is_empty() {
                    collect_dir_files(
                        &canonical_root,
                        &canonical_root,
                        &mut collected_rel_paths,
                        &mut seen,
                        MAX_FULL_SNAPSHOT_FILES,
                    )?;
                    snapshotted_dirs.push(String::new());
                } else if abs.is_dir() {
                    snapshotted_dirs.push(rel_str.clone());
                    collect_dir_files(
                        &canonical_root,
                        &abs,
                        &mut collected_rel_paths,
                        &mut seen,
                        MAX_FULL_SNAPSHOT_FILES,
                    )?;
                } else if seen.insert(rel_str.clone()) {
                    collected_rel_paths.push(rel_str);
                }
            }
        }

        let mut entries = Vec::with_capacity(collected_rel_paths.len());
        for rel_str in collected_rel_paths {
            let src_path = canonical_root.join(&rel_str);
            if src_path.is_file() {
                let dst_path = files_dir.join(&rel_str);
                if let Some(parent) = dst_path.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|e| format!("Failed to create snapshot parent dir: {e}"))?;
                }
                fs::copy(&src_path, &dst_path)
                    .map_err(|e| format!("Failed to copy '{}' to snapshot: {e}", rel_str))?;
                entries.push(CheckpointFileEntry {
                    rel_path: rel_str.clone(),
                    existed: true,
                    snapshot_rel: Some(format!("files/{rel_str}")),
                });
            } else if !src_path.exists() {
                entries.push(CheckpointFileEntry {
                    rel_path: rel_str,
                    existed: false,
                    snapshot_rel: None,
                });
            }
        }

        let (git_status, git_head) = capture_git_metadata(&canonical_root);

        let manifest = CheckpointManifest {
            checkpoint_id: checkpoint_id.clone(),
            label: label.to_string(),
            created_at: now_secs,
            workspace_root: canonical_root.to_string_lossy().to_string(),
            full_workspace,
            snapshotted_dirs,
            git_status: git_status.clone(),
            git_head: git_head.clone(),
            files: entries,
        };

        let manifest_json = serde_json::to_string_pretty(&manifest)
            .map_err(|e| format!("Failed to serialize checkpoint manifest: {e}"))?;
        fs::write(cp_dir.join("manifest.json"), manifest_json)
            .map_err(|e| format!("Failed to write checkpoint manifest: {e}"))?;

        Ok(CheckpointSummary {
            checkpoint_id,
            label: manifest.label,
            created_at: manifest.created_at,
            file_count: manifest.files.len(),
            git_status,
            git_head,
        })
    }

    /// Registers a newly created file in an existing checkpoint manifest (if not already present)
    /// so restoring that checkpoint removes the newly created file.
    pub fn track_created_file(
        &self,
        workspace_root: &Path,
        checkpoint_id: &str,
        path: &Path,
    ) -> Result<(), String> {
        let canonical_root = workspace_root
            .canonicalize()
            .unwrap_or_else(|_| workspace_root.to_path_buf());
        let cp_dir = self.checkpoint_dir(&canonical_root, checkpoint_id)?;
        let manifest_path = cp_dir.join("manifest.json");
        let raw = fs::read_to_string(&manifest_path)
            .map_err(|e| format!("Failed to read checkpoint '{checkpoint_id}': {e}"))?;
        let mut manifest: CheckpointManifest = serde_json::from_str(&raw)
            .map_err(|e| format!("Corrupt checkpoint manifest: {e}"))?;

        let abs = resolve_target_path(&canonical_root, path)?;
        let rel = abs
            .strip_prefix(&canonical_root)
            .map_err(|_| format!("Path escapes workspace: {}", path.display()))?;
        let rel_str = normalize_rel_path(rel);
        if rel_str.is_empty() {
            return Err("Cannot track workspace root as a created file.".into());
        }

        if !manifest.files.iter().any(|f| f.rel_path == rel_str) {
            manifest.files.push(CheckpointFileEntry {
                rel_path: rel_str,
                existed: false,
                snapshot_rel: None,
            });
            let updated = serde_json::to_string_pretty(&manifest)
                .map_err(|e| format!("Failed to serialize checkpoint manifest: {e}"))?;
            fs::write(&manifest_path, updated)
                .map_err(|e| format!("Failed to update checkpoint manifest: {e}"))?;
        }
        Ok(())
    }

    /// Lists all checkpoints recorded for `workspace_root`, ordered newest first.
    pub fn list_checkpoints(&self, workspace_root: &Path) -> Vec<CheckpointSummary> {
        let canonical_root = workspace_root
            .canonicalize()
            .unwrap_or_else(|_| workspace_root.to_path_buf());
        let dir = self.workspace_dir(&canonical_root);
        let Ok(entries) = fs::read_dir(dir) else {
            return Vec::new();
        };

        let mut items = Vec::new();
        for entry in entries.flatten() {
            let manifest_path = entry.path().join("manifest.json");
            let Ok(raw) = fs::read_to_string(&manifest_path) else {
                continue;
            };
            let Ok(manifest) = serde_json::from_str::<CheckpointManifest>(&raw) else {
                continue;
            };
            items.push(CheckpointSummary {
                checkpoint_id: manifest.checkpoint_id,
                label: manifest.label,
                created_at: manifest.created_at,
                file_count: manifest.files.len(),
                git_status: manifest.git_status,
                git_head: manifest.git_head,
            });
        }

        items.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.checkpoint_id.cmp(&a.checkpoint_id))
        });
        items
    }

    /// Restores modified files from the checkpoint snapshot and deletes files that were newly
    /// created after the snapshot was taken.
    pub fn restore_checkpoint(
        &self,
        workspace_root: &Path,
        checkpoint_id: &str,
    ) -> Result<RestoreResult, String> {
        let canonical_root = workspace_root
            .canonicalize()
            .unwrap_or_else(|_| workspace_root.to_path_buf());
        let cp_dir = self.checkpoint_dir(&canonical_root, checkpoint_id)?;
        let manifest_path = cp_dir.join("manifest.json");
        let raw = fs::read_to_string(&manifest_path)
            .map_err(|e| format!("Checkpoint '{checkpoint_id}' not found: {e}"))?;
        let manifest: CheckpointManifest = serde_json::from_str(&raw)
            .map_err(|e| format!("Corrupt checkpoint manifest: {e}"))?;

        let (git_status_before, _) = capture_git_metadata(&canonical_root);

        let mut restored_files = Vec::new();
        let mut removed_files = Vec::new();
        let mut known_rel_paths = HashSet::new();

        for entry in &manifest.files {
            known_rel_paths.insert(entry.rel_path.clone());
            let target_path = resolve_target_path(&canonical_root, Path::new(&entry.rel_path))?;

            if entry.existed {
                let snapshot_rel = entry
                    .snapshot_rel
                    .as_deref()
                    .ok_or_else(|| format!("Missing snapshot file reference for {}", entry.rel_path))?;
                let snapshot_path = cp_dir.join(snapshot_rel);
                if !snapshot_path.is_file() {
                    return Err(format!(
                        "Snapshot artifact missing for '{}': {}",
                        entry.rel_path,
                        snapshot_path.display()
                    ));
                }
                if let Some(parent) = target_path.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|e| format!("Failed to create parent directory: {e}"))?;
                }
                fs::copy(&snapshot_path, &target_path).map_err(|e| {
                    format!("Failed to restore file '{}': {e}", entry.rel_path)
                })?;
                restored_files.push(entry.rel_path.clone());
            } else if target_path.exists() {
                fs::remove_file(&target_path).map_err(|e| {
                    format!("Failed to remove newly created file '{}': {e}", entry.rel_path)
                })?;
                removed_files.push(entry.rel_path.clone());
            }
        }

        // If this checkpoint covered the full workspace or whole directories, remove any new files
        // created inside that scope since the snapshot was taken.
        let mut scope_dirs: Vec<PathBuf> = Vec::new();
        if manifest.full_workspace {
            scope_dirs.push(canonical_root.clone());
        } else {
            for d in &manifest.snapshotted_dirs {
                let dir = if d.is_empty() {
                    canonical_root.clone()
                } else {
                    canonical_root.join(d)
                };
                if dir.is_dir() {
                    scope_dirs.push(dir);
                }
            }
        }

        if !scope_dirs.is_empty() {
            let mut current_files = Vec::new();
            let mut seen = HashSet::new();
            for dir in scope_dirs {
                collect_dir_files(
                    &canonical_root,
                    &dir,
                    &mut current_files,
                    &mut seen,
                    MAX_FULL_SNAPSHOT_FILES * 2,
                )?;
            }
            for rel_str in current_files {
                if !known_rel_paths.contains(&rel_str) {
                    let target_path = resolve_target_path(&canonical_root, Path::new(&rel_str))?;
                    if target_path.is_file() {
                        fs::remove_file(&target_path).map_err(|e| {
                            format!("Failed to remove untracked file '{}': {e}", rel_str)
                        })?;
                        removed_files.push(rel_str);
                    }
                }
            }
        }

        Ok(RestoreResult {
            checkpoint_id: manifest.checkpoint_id,
            restored_files,
            removed_files,
            git_status_before,
        })
    }
}

/// Convenience wrapper using the default `~/.rursor/checkpoints` directory.
pub fn create_checkpoint(
    workspace_root: &Path,
    label: &str,
    target_paths: &[PathBuf],
) -> Result<CheckpointSummary, String> {
    CheckpointStore::default_store().create_checkpoint(workspace_root, label, target_paths)
}

/// Convenience wrapper listing checkpoints from the default `~/.rursor/checkpoints` directory.
pub fn list_checkpoints(workspace_root: &Path) -> Vec<CheckpointSummary> {
    CheckpointStore::default_store().list_checkpoints(workspace_root)
}

/// Convenience wrapper restoring a checkpoint from the default `~/.rursor/checkpoints` directory.
pub fn restore_checkpoint(
    workspace_root: &Path,
    checkpoint_id: &str,
) -> Result<RestoreResult, String> {
    CheckpointStore::default_store().restore_checkpoint(workspace_root, checkpoint_id)
}

/// Creates an isolated git worktree sandbox on a new branch `rursor/<task_slug>-<seq>` under
/// `~/.rursor/worktrees/<repo_hash>/`.
pub fn create_worktree(repo_root: &Path, task_name: &str) -> Result<WorktreeInfo, String> {
    let canonical_root = repo_root
        .canonicalize()
        .map_err(|e| format!("Invalid repository root '{}': {e}", repo_root.display()))?;
    ensure_git_repo(&canonical_root)?;

    let slug = sanitize_slug(task_name);
    let seq = NEXT_WORKTREE_SEQ.fetch_add(1, Ordering::Relaxed);
    let ts = epoch_secs();
    let branch_name = format!("rursor/{slug}-{ts}-{seq}");
    let worktree_dir = rursor_home_dir()
        .join("worktrees")
        .join(workspace_hash(&canonical_root))
        .join(format!("{slug}-{ts}-{seq}"));

    if let Some(parent) = worktree_dir.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create worktrees parent directory: {e}"))?;
    }

    let wt_str = worktree_dir.to_string_lossy().to_string();
    run_git(
        &canonical_root,
        &["worktree", "add", "-b", &branch_name, &wt_str],
    )?;

    let (_, head) = capture_git_metadata(&worktree_dir);
    let final_path = worktree_dir
        .canonicalize()
        .unwrap_or(worktree_dir)
        .to_string_lossy()
        .to_string();

    Ok(WorktreeInfo {
        path: final_path,
        branch: Some(branch_name),
        head,
        is_main: false,
        is_bare: false,
    })
}

/// Lists all git worktrees registered in `repo_root`.
pub fn list_worktrees(repo_root: &Path) -> Result<Vec<WorktreeInfo>, String> {
    let canonical_root = repo_root
        .canonicalize()
        .map_err(|e| format!("Invalid repository root '{}': {e}", repo_root.display()))?;
    ensure_git_repo(&canonical_root)?;

    let raw = run_git(&canonical_root, &["worktree", "list", "--porcelain"])?;
    Ok(parse_worktree_porcelain(&raw))
}

/// Returns the combined status and diff for changes inside `worktree_path`.
pub fn diff_worktree(worktree_path: &Path) -> Result<String, String> {
    if !worktree_path.exists() {
        return Err(format!(
            "Worktree path does not exist: {}",
            worktree_path.display()
        ));
    }
    ensure_git_repo(worktree_path)?;

    let status = run_git(worktree_path, &["status", "--short"])?;
    let diff = match run_git(worktree_path, &["diff", "HEAD"]) {
        Ok(d) => d,
        Err(_) => run_git(worktree_path, &["diff"])?,
    };

    let mut sections = Vec::new();
    if !status.trim().is_empty() {
        sections.push(format!("## Status\n{}", status.trim_end()));
    }
    if !diff.trim().is_empty() {
        sections.push(format!("## Diff\n{}", diff.trim_end()));
    }
    if sections.is_empty() {
        Ok("No uncommitted changes in worktree.".to_string())
    } else {
        Ok(sections.join("\n\n"))
    }
}

/// Merges `branch_name` into the currently checked-out branch in `repo_root`.
pub fn merge_worktree(repo_root: &Path, branch_name: &str) -> Result<String, String> {
    let canonical_root = repo_root
        .canonicalize()
        .map_err(|e| format!("Invalid repository root '{}': {e}", repo_root.display()))?;
    ensure_git_repo(&canonical_root)?;

    let msg = format!("Merge rursor worktree branch {branch_name}");
    run_git(&canonical_root, &["merge", "--no-ff", branch_name, "-m", &msg])
}

/// Removes a git worktree at `worktree_path` from `repo_root` and prunes stale entries.
pub fn remove_worktree(repo_root: &Path, worktree_path: &Path) -> Result<(), String> {
    let canonical_root = repo_root
        .canonicalize()
        .map_err(|e| format!("Invalid repository root '{}': {e}", repo_root.display()))?;
    ensure_git_repo(&canonical_root)?;

    let wt_str = worktree_path.to_string_lossy().to_string();
    run_git(&canonical_root, &["worktree", "remove", "--force", &wt_str])?;
    let _ = run_git(&canonical_root, &["worktree", "prune"]);
    Ok(())
}

fn parse_worktree_porcelain(raw: &str) -> Vec<WorktreeInfo> {
    let mut worktrees = Vec::new();
    let mut current: Option<WorktreeInfo> = None;

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if let Some(wt) = current.take() {
                worktrees.push(wt);
            }
            continue;
        }

        if let Some(path) = trimmed.strip_prefix("worktree ") {
            if let Some(wt) = current.take() {
                worktrees.push(wt);
            }
            let is_main = worktrees.is_empty();
            current = Some(WorktreeInfo {
                path: path.to_string(),
                branch: None,
                head: None,
                is_main,
                is_bare: false,
            });
        } else if let Some(wt) = current.as_mut() {
            if let Some(head) = trimmed.strip_prefix("HEAD ") {
                wt.head = Some(head.to_string());
            } else if let Some(branch) = trimmed.strip_prefix("branch ") {
                let short = branch.strip_prefix("refs/heads/").unwrap_or(branch);
                wt.branch = Some(short.to_string());
            } else if trimmed == "bare" {
                wt.is_bare = true;
            }
        }
    }

    if let Some(wt) = current.take() {
        worktrees.push(wt);
    }

    worktrees
}

fn ensure_git_repo(dir: &Path) -> Result<(), String> {
    let out = run_git(dir, &["rev-parse", "--is-inside-work-tree"])?;
    if out.trim() != "true" {
        return Err(format!("Not a git work tree: {}", dir.display()));
    }
    Ok(())
}

fn run_git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| format!("Failed to invoke git {:?}: {e}", args))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if !output.status.success() {
        let detail = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        return Err(format!("git {} failed: {detail}", args.join(" ")));
    }
    Ok(stdout)
}

fn capture_git_metadata(workspace_root: &Path) -> (Option<String>, Option<String>) {
    if ensure_git_repo(workspace_root).is_err() {
        return (None, None);
    }
    let status = run_git(workspace_root, &["status", "--porcelain"])
        .ok()
        .map(|s| s.trim_end().to_string());
    let head = run_git(workspace_root, &["rev-parse", "--short", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    (status, head)
}

fn resolve_target_path(canonical_root: &Path, path: &Path) -> Result<PathBuf, String> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        canonical_root.join(path)
    };

    let normalized = normalize_lexical(&joined);
    if !normalized.starts_with(canonical_root) {
        return Err(format!(
            "Path '{}' escapes workspace root '{}'",
            path.display(),
            canonical_root.display()
        ));
    }
    Ok(normalized)
}

fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::Prefix(p) => out.push(p.as_os_str()),
            Component::RootDir => out.push(comp.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

fn collect_dir_files(
    canonical_root: &Path,
    dir: &Path,
    out: &mut Vec<String>,
    seen: &mut HashSet<String>,
    limit: usize,
) -> Result<(), String> {
    if out.len() >= limit {
        return Ok(());
    }
    let entries = fs::read_dir(dir)
        .map_err(|e| format!("Failed to read directory '{}': {e}", dir.display()))?;
    for entry in entries.flatten() {
        if out.len() >= limit {
            break;
        }
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if path.is_dir() {
            if IGNORED_DIRS.contains(&name_str.as_ref()) {
                continue;
            }
            collect_dir_files(canonical_root, &path, out, seen, limit)?;
        } else if path.is_file() {
            if let Ok(rel) = path.strip_prefix(canonical_root) {
                let rel_str = normalize_rel_path(rel);
                if seen.insert(rel_str.clone()) {
                    out.push(rel_str);
                }
            }
        }
    }
    Ok(())
}

fn normalize_rel_path(rel: &Path) -> String {
    rel.components()
        .filter_map(|c| match c {
            Component::Normal(part) => Some(part.to_string_lossy().to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn rursor_home_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".rursor")
}

fn workspace_hash(workspace_root: &Path) -> String {
    let mut hasher = DefaultHasher::new();
    workspace_root.to_string_lossy().hash(&mut hasher);
    format!("ws_{:016x}", hasher.finish())
}

fn sanitize_id(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect()
}

fn sanitize_slug(raw: &str) -> String {
    let slug: String = raw
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = slug.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "task".to_string()
    } else {
        trimmed.chars().take(32).collect()
    }
}

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn epoch_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("{prefix}_{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn creates_lists_and_restores_checkpoint_with_rollback() {
        let ws = temp_dir("rursor_cp_ws");
        let store_dir = temp_dir("rursor_cp_store");
        let store = CheckpointStore::with_dir(store_dir.clone());

        fs::create_dir_all(ws.join("src")).unwrap();
        fs::write(ws.join("src/lib.rs"), "pub fn v() -> i32 { 1 }\n").unwrap();
        fs::write(ws.join("README.md"), "# Initial\n").unwrap();

        let summary = store
            .create_checkpoint(
                &ws,
                "before edit",
                &[
                    PathBuf::from("src/lib.rs"),
                    PathBuf::from("src/new_file.rs"),
                ],
            )
            .unwrap();
        assert_eq!(summary.label, "before edit");
        assert_eq!(summary.file_count, 2);

        // Mutate tracked existing file and create tracked non-existent file
        fs::write(ws.join("src/lib.rs"), "pub fn v() -> i32 { 99 }\n").unwrap();
        fs::write(ws.join("src/new_file.rs"), "pub fn extra() {}\n").unwrap();

        let listed = store.list_checkpoints(&ws);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].checkpoint_id, summary.checkpoint_id);

        let restored = store.restore_checkpoint(&ws, &summary.checkpoint_id).unwrap();
        assert_eq!(restored.restored_files, vec!["src/lib.rs".to_string()]);
        assert_eq!(restored.removed_files, vec!["src/new_file.rs".to_string()]);

        assert_eq!(
            fs::read_to_string(ws.join("src/lib.rs")).unwrap(),
            "pub fn v() -> i32 { 1 }\n"
        );
        assert!(!ws.join("src/new_file.rs").exists());
        assert_eq!(
            fs::read_to_string(ws.join("README.md")).unwrap(),
            "# Initial\n"
        );

        let _ = fs::remove_dir_all(&ws);
        let _ = fs::remove_dir_all(&store_dir);
    }

    #[test]
    fn full_workspace_checkpoint_removes_untracked_post_snapshot_files() {
        let ws = temp_dir("rursor_cp_full_ws");
        let store_dir = temp_dir("rursor_cp_full_store");
        let store = CheckpointStore::with_dir(store_dir.clone());

        fs::write(ws.join("a.txt"), "hello").unwrap();
        let cp = store.create_checkpoint(&ws, "full snap", &[]).unwrap();
        assert_eq!(cp.file_count, 1);

        fs::write(ws.join("a.txt"), "changed").unwrap();
        fs::write(ws.join("created_later.txt"), "remove me").unwrap();

        let res = store.restore_checkpoint(&ws, &cp.checkpoint_id).unwrap();
        assert_eq!(res.restored_files, vec!["a.txt".to_string()]);
        assert_eq!(res.removed_files, vec!["created_later.txt".to_string()]);
        assert_eq!(fs::read_to_string(ws.join("a.txt")).unwrap(), "hello");
        assert!(!ws.join("created_later.txt").exists());

        let _ = fs::remove_dir_all(&ws);
        let _ = fs::remove_dir_all(&store_dir);
    }

    #[test]
    fn checkpoint_captures_git_status_and_worktree_sandbox_lifecycle() {
        let repo = temp_dir("rursor_git_wt");
        let store_dir = temp_dir("rursor_git_cp_store");
        let store = CheckpointStore::with_dir(store_dir.clone());

        run_git(&repo, &["init"]).unwrap();
        run_git(&repo, &["config", "user.email", "test@rursor.local"]).unwrap();
        run_git(&repo, &["config", "user.name", "Rursor Test"]).unwrap();
        fs::write(repo.join("main.txt"), "base\n").unwrap();
        run_git(&repo, &["add", "main.txt"]).unwrap();
        run_git(&repo, &["commit", "-m", "initial"]).unwrap();

        // Dirty file so git status metadata is non-empty
        fs::write(repo.join("main.txt"), "dirty\n").unwrap();
        let cp = store.create_checkpoint(&repo, "git meta", &[]).unwrap();
        assert!(cp.git_head.is_some());
        assert!(cp.git_status.as_deref().unwrap_or("").contains("main.txt"));

        // Restore clean state before worktree test
        store.restore_checkpoint(&repo, &cp.checkpoint_id).unwrap();
        fs::write(repo.join("main.txt"), "base\n").unwrap();

        let wt = create_worktree(&repo, "Fix Auth Bug").unwrap();
        let wt_path = PathBuf::from(&wt.path);
        assert!(wt_path.exists());
        let branch = wt.branch.clone().unwrap();
        assert!(branch.starts_with("rursor/fix-auth-bug-"));

        let listed = list_worktrees(&repo).unwrap();
        assert!(listed.len() >= 2);
        assert!(listed.iter().any(|w| w.branch.as_deref() == Some(branch.as_str())));

        fs::write(wt_path.join("main.txt"), "base\nfixed in worktree\n").unwrap();
        let diff = diff_worktree(&wt_path).unwrap();
        assert!(diff.contains("fixed in worktree"));

        run_git(&wt_path, &["add", "main.txt"]).unwrap();
        run_git(&wt_path, &["commit", "-m", "worktree fix"]).unwrap();

        let merge_out = merge_worktree(&repo, &branch).unwrap();
        assert!(!merge_out.is_empty() || fs::read_to_string(repo.join("main.txt")).unwrap().contains("fixed in worktree"));
        assert!(fs::read_to_string(repo.join("main.txt"))
            .unwrap()
            .contains("fixed in worktree"));

        remove_worktree(&repo, &wt_path).unwrap();
        assert!(!wt_path.exists());

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&store_dir);
    }
}
