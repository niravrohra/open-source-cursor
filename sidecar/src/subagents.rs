use crate::gemini::GeminiClient;
use crate::protocol::{ChatMessage, ContextSnippet};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::Path;
use tokio::task::JoinSet;

const MAX_VERIFY_OUTPUT_CHARS: usize = 4000;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentTask {
    pub id: String,
    pub role: String,
    pub prompt: String,
    #[serde(default)]
    pub context_snippets: Vec<ContextSnippet>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SubagentResult {
    pub id: String,
    pub role: String,
    pub ok: bool,
    pub output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct VerifyCommand {
    pub label: String,
    pub command: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct VerifyResult {
    pub label: String,
    pub command: String,
    pub exit_code: i32,
    pub output: String,
}

/// Executes multiple read-only/research/analysis subtasks concurrently against Gemini using
/// `tokio::task::JoinSet`, invoking `on_progress(task_id, role, status, output)` as tasks progress.
pub async fn run_parallel_subagents<F>(
    gemini: &GeminiClient,
    api_key: &str,
    model: &str,
    system_prompt: Option<&str>,
    tasks: Vec<SubagentTask>,
    mut on_progress: F,
) -> Vec<SubagentResult>
where
    F: FnMut(&str, &str, &str, Option<&str>),
{
    if tasks.is_empty() {
        return Vec::new();
    }

    let task_meta: Vec<(String, String)> = tasks
        .iter()
        .map(|t| (t.id.clone(), t.role.clone()))
        .collect();
    let mut ordered: Vec<Option<SubagentResult>> = vec![None; tasks.len()];
    let mut join_set: JoinSet<(usize, SubagentResult)> = JoinSet::new();

    for (idx, task) in tasks.into_iter().enumerate() {
        on_progress(&task.id, &task.role, "started", None);

        let client = gemini.clone();
        let api_key_owned = api_key.to_string();
        let model_owned = model.to_string();
        let base_sys = system_prompt.map(|s| s.to_string());

        join_set.spawn(async move {
            let subagent_sys = build_subagent_system_prompt(base_sys.as_deref(), &task.role);
            let user_prompt = build_subagent_user_prompt(&task);
            let messages = vec![ChatMessage {
                role: "user".to_string(),
                content: user_prompt,
                images: Vec::new(),
            }];

            let res = match client
                .stream_chat(
                    &api_key_owned,
                    &model_owned,
                    Some(&subagent_sys),
                    &messages,
                    0.2,
                    Some(4096),
                    |_| {},
                )
                .await
            {
                Ok(text) => SubagentResult {
                    id: task.id,
                    role: task.role,
                    ok: true,
                    output: text,
                },
                Err(err) => SubagentResult {
                    id: task.id,
                    role: task.role,
                    ok: false,
                    output: err,
                },
            };
            (idx, res)
        });
    }

    while let Some(joined) = join_set.join_next().await {
        match joined {
            Ok((idx, result)) => {
                let status = if result.ok { "completed" } else { "failed" };
                on_progress(&result.id, &result.role, status, Some(&result.output));
                if idx < ordered.len() {
                    ordered[idx] = Some(result);
                }
            }
            Err(join_err) => {
                on_progress("unknown", "worker", &format!("failed: {join_err}"), None);
            }
        }
    }

    ordered
        .into_iter()
        .enumerate()
        .map(|(idx, item)| {
            item.unwrap_or_else(|| {
                let (id, role) = &task_meta[idx];
                SubagentResult {
                    id: id.clone(),
                    role: role.clone(),
                    ok: false,
                    output: "Subagent task did not return a result.".to_string(),
                }
            })
        })
        .collect()
}

fn build_subagent_system_prompt(base_system: Option<&str>, role: &str) -> String {
    let role_instruction = format!(
        "You are a focused read-only analysis subagent assigned the role: '{role}'. \
         Analyze the provided prompt and context snippets thoroughly and concisely. \
         Do not invent file paths or symbols not present in the context; cite exact file paths and symbol names."
    );
    match base_system {
        Some(base) if !base.trim().is_empty() => format!("{base}\n\n{role_instruction}"),
        _ => role_instruction,
    }
}

fn build_subagent_user_prompt(task: &SubagentTask) -> String {
    let mut out = String::new();
    out.push_str(&format!("## Subtask Role: {}\n", task.role));
    out.push_str(&format!("## Task\n{}\n", task.prompt));

    if !task.context_snippets.is_empty() {
        out.push_str("\n## Context Snippets\n");
        for snippet in &task.context_snippets {
            out.push_str(&format!(
                "### {}\n```\n{}\n```\n\n",
                snippet.path, snippet.content
            ));
        }
    }
    out
}

/// Inspects `workspace_root` for standard project manifests and returns ordered verification
/// commands (`cargo test`, `npm run typecheck`/`tsc`/`npm test`, `pytest`, `go test ./...`).
pub fn detect_verify_commands(workspace_root: &Path) -> Vec<VerifyCommand> {
    let mut commands = Vec::new();

    if workspace_root.join("Cargo.toml").is_file() {
        commands.push(VerifyCommand {
            label: "Rust tests".to_string(),
            command: "cargo test".to_string(),
        });
    }

    let pkg_path = workspace_root.join("package.json");
    if pkg_path.is_file() {
        let pkg_json = fs::read_to_string(&pkg_path)
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok());

        let scripts = pkg_json
            .as_ref()
            .and_then(|v| v.get("scripts"))
            .and_then(|s| s.as_object());

        let has_typecheck_script = scripts
            .and_then(|s| s.get("typecheck"))
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.trim().is_empty());

        if has_typecheck_script {
            commands.push(VerifyCommand {
                label: "TypeScript typecheck".to_string(),
                command: "npm run typecheck".to_string(),
            });
        } else if workspace_root.join("tsconfig.json").is_file() {
            commands.push(VerifyCommand {
                label: "TypeScript compiler check".to_string(),
                command: "npx tsc --noEmit".to_string(),
            });
        }

        let has_real_test_script = scripts
            .and_then(|s| s.get("test"))
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.trim().is_empty() && !s.contains("no test specified"));

        if has_real_test_script {
            commands.push(VerifyCommand {
                label: "Node tests".to_string(),
                command: "npm test".to_string(),
            });
        }
    } else if workspace_root.join("tsconfig.json").is_file() {
        commands.push(VerifyCommand {
            label: "TypeScript compiler check".to_string(),
            command: "npx tsc --noEmit".to_string(),
        });
    }

    if workspace_root.join("pyproject.toml").is_file() || workspace_root.join("pytest.ini").is_file()
    {
        commands.push(VerifyCommand {
            label: "Python pytest".to_string(),
            command: "pytest".to_string(),
        });
    }

    if workspace_root.join("go.mod").is_file() {
        commands.push(VerifyCommand {
            label: "Go tests".to_string(),
            command: "go test ./...".to_string(),
        });
    }

    commands
}

/// Returns `true` if all verification results exited with code 0.
pub fn all_verify_passed(results: &[VerifyResult]) -> bool {
    results.iter().all(|r| r.exit_code == 0)
}

/// Formats verification execution results into concise Markdown feedback for an autonomous agent.
pub fn format_verify_feedback(results: &[VerifyResult]) -> String {
    if results.is_empty() {
        return "No verification commands were executed.".to_string();
    }

    let passed = results.iter().filter(|r| r.exit_code == 0).count();
    let failed = results.len() - passed;

    let mut out = String::new();
    if failed == 0 {
        out.push_str(&format!(
            "Verification PASSED ({passed}/{} checks succeeded).\n",
            results.len()
        ));
    } else {
        out.push_str(&format!(
            "Verification FAILED ({failed} failed, {passed} passed out of {} checks).\n",
            results.len()
        ));
    }

    for res in results {
        let badge = if res.exit_code == 0 { "PASS" } else { "FAIL" };
        out.push_str(&format!(
            "\n### [{badge}] {} (`{}`) — exit code {}\n",
            res.label, res.command, res.exit_code
        ));

        let trimmed = res.output.trim();
        if trimmed.is_empty() {
            out.push_str("(no output)\n");
        } else {
            let excerpt = tail_chars(trimmed, MAX_VERIFY_OUTPUT_CHARS);
            out.push_str("```\n");
            out.push_str(&excerpt);
            out.push_str("\n```\n");
        }
    }

    out
}

fn tail_chars(text: &str, max_chars: usize) -> String {
    let char_count = text.chars().count();
    if char_count <= max_chars {
        return text.to_string();
    }
    let skip = char_count - max_chars;
    let tail: String = text.chars().skip(skip).collect();
    format!("...(truncated {skip} leading chars)...\n{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

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
    fn detects_cargo_node_python_and_go_verify_commands() {
        let dir = temp_dir("rursor_verify_detect");

        assert!(detect_verify_commands(&dir).is_empty());

        fs::write(dir.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
        fs::write(
            dir.join("package.json"),
            r#"{"name":"demo","scripts":{"typecheck":"tsc --noEmit","test":"vitest run"}}"#,
        )
        .unwrap();
        fs::write(dir.join("pyproject.toml"), "[project]\nname = \"demo\"\n").unwrap();
        fs::write(dir.join("go.mod"), "module example.com/demo\n").unwrap();

        let cmds = detect_verify_commands(&dir);
        let raw_cmds: Vec<&str> = cmds.iter().map(|c| c.command.as_str()).collect();
        assert_eq!(
            raw_cmds,
            vec![
                "cargo test",
                "npm run typecheck",
                "npm test",
                "pytest",
                "go test ./..."
            ]
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn detects_tsconfig_fallback_and_ignores_default_npm_test_stub() {
        let dir = temp_dir("rursor_verify_tsconfig");

        fs::write(
            dir.join("package.json"),
            r#"{"name":"ext","scripts":{"test":"echo \"Error: no test specified\" && exit 1"}}"#,
        )
        .unwrap();
        fs::write(dir.join("tsconfig.json"), "{}").unwrap();
        fs::write(dir.join("pytest.ini"), "[pytest]\n").unwrap();

        let cmds = detect_verify_commands(&dir);
        let raw_cmds: Vec<&str> = cmds.iter().map(|c| c.command.as_str()).collect();
        assert_eq!(raw_cmds, vec!["npx tsc --noEmit", "pytest"]);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn formats_verify_feedback_for_passed_and_failed_checks() {
        assert_eq!(
            format_verify_feedback(&[]),
            "No verification commands were executed."
        );

        let results = vec![
            VerifyResult {
                label: "TypeScript typecheck".to_string(),
                command: "npm run typecheck".to_string(),
                exit_code: 0,
                output: "0 errors".to_string(),
            },
            VerifyResult {
                label: "Rust tests".to_string(),
                command: "cargo test".to_string(),
                exit_code: 101,
                output: "error[E0425]: cannot find value `x` in this scope".to_string(),
            },
        ];

        assert!(!all_verify_passed(&results));
        let feedback = format_verify_feedback(&results);
        assert!(feedback.contains("Verification FAILED (1 failed, 1 passed out of 2 checks)."));
        assert!(feedback.contains("[PASS] TypeScript typecheck (`npm run typecheck`) — exit code 0"));
        assert!(feedback.contains("[FAIL] Rust tests (`cargo test`) — exit code 101"));
        assert!(feedback.contains("error[E0425]"));
    }
}
