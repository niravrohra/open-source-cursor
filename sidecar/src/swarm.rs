#![allow(dead_code)]

use crate::gemini::{chat_message_to_gemini_content, GeminiClient, GeminiTurnOptions};
use crate::macos_sandbox::{
    run_sandboxed_command_async, ApfsCloneManager, CloneFileDiff, PermissionProfile,
};
use crate::protocol::ChatMessage;
use crate::router::{
    route_task, stream_anthropic_chat, stream_openai_chat, ProviderKeys, ProviderKind,
    RouteDecision, TaskKind,
};
use crate::subagents::{
    all_verify_passed, detect_verify_commands, format_verify_feedback, VerifyResult,
};
use crate::syntax_gate::validate_edit_syntax;
use reqwest::Client;
use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

const DEFAULT_WORKER_MAX_TURNS: usize = 8;
const DEFAULT_MAX_PARALLEL_WORKERS: usize = 4;
const DEFAULT_COMMAND_TIMEOUT_SECS: u64 = 90;
const DEFAULT_VERIFY_TIMEOUT_SECS: u64 = 120;
const MAX_TOOL_READ_LINES: usize = 260;
const MAX_TOOL_OUTPUT_CHARS: usize = 6_000;
const IGNORED_WALK_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "out",
    "dist",
    ".next",
    ".vscode-src",
    "VSCode-darwin-arm64",
];

static NEXT_SWARM_SEQ: AtomicU64 = AtomicU64::new(1);

// ============================================================================
// 1. Data Models (`#[serde(rename_all = "camelCase")]`)
// ============================================================================

/// Specialized or custom role assigned to a Swarm worker node.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum SwarmRole {
    Planner,
    Researcher,
    #[default]
    Coder,
    Verifier,
    MediaProducer,
    Custom(String),
}

impl SwarmRole {
    pub fn from_str_role(raw: &str) -> Self {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Self::Coder;
        }
        match trimmed.to_ascii_lowercase().as_str() {
            "planner" | "plan" | "architect" => Self::Planner,
            "researcher" | "research" | "explorer" | "analyst" => Self::Researcher,
            "coder" | "implementer" | "developer" | "engineer" | "builder" => Self::Coder,
            "verifier" | "tester" | "qa" | "reviewer" | "validator" => Self::Verifier,
            "mediaproducer" | "media_producer" | "media-producer" | "media" | "designer" => {
                Self::MediaProducer
            }
            _ => Self::Custom(trimmed.to_string()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Planner => "planner",
            Self::Researcher => "researcher",
            Self::Coder => "coder",
            Self::Verifier => "verifier",
            Self::MediaProducer => "mediaProducer",
            Self::Custom(s) => s.as_str(),
        }
    }
}

impl fmt::Display for SwarmRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for SwarmRole {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for SwarmRole {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct SwarmRoleVisitor;

        impl<'de> Visitor<'de> for SwarmRoleVisitor {
            type Value = SwarmRole;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(
                    "a swarm role string (planner, researcher, coder, verifier, mediaProducer, or custom)",
                )
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(SwarmRole::from_str_role(value))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(SwarmRole::from_str_role(&value))
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                if let Some((key, val)) = map.next_entry::<String, String>()? {
                    if key.eq_ignore_ascii_case("custom") || key.eq_ignore_ascii_case("role") {
                        return Ok(SwarmRole::from_str_role(&val));
                    }
                    return Ok(SwarmRole::from_str_role(&key));
                }
                Err(de::Error::custom("empty object for SwarmRole"))
            }
        }

        deserializer.deserialize_any(SwarmRoleVisitor)
    }
}

/// Static specification of a single task node in a Swarm execution DAG.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SwarmTaskSpec {
    pub id: String,
    pub title: String,
    pub role: SwarmRole,
    pub prompt: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub write_enabled: bool,
    #[serde(default)]
    pub model_override: Option<String>,
}

/// Execution lifecycle status of a `SwarmNodeState`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum SwarmNodeStatus {
    #[default]
    Pending,
    Running,
    Verifying,
    Completed,
    Failed,
    Blocked,
}

/// Live and final execution state for a single Swarm DAG node.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SwarmNodeState {
    pub id: String,
    pub title: String,
    pub role: SwarmRole,
    #[serde(default)]
    pub depends_on: Vec<String>,
    pub status: SwarmNodeStatus,
    pub clone_id: Option<String>,
    pub clone_path: Option<String>,
    pub summary: String,
    #[serde(default)]
    pub modified_files: Vec<CloneFileDiff>,
    #[serde(default)]
    pub verify_results: Vec<VerifyResult>,
    pub duration_ms: u64,
    pub error: Option<String>,
}

impl SwarmNodeState {
    pub fn from_spec(spec: &SwarmTaskSpec) -> Self {
        Self {
            id: spec.id.clone(),
            title: spec.title.clone(),
            role: spec.role.clone(),
            depends_on: spec.depends_on.clone(),
            status: SwarmNodeStatus::Pending,
            clone_id: None,
            clone_path: None,
            summary: String::new(),
            modified_files: Vec::new(),
            verify_results: Vec::new(),
            duration_ms: 0,
            error: None,
        }
    }
}

/// Shared key-value entry published by a worker onto `SwarmBlackboard`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BlackboardEntry {
    pub key: String,
    pub value: String,
    pub author_id: String,
    pub updated_at: u64,
}

/// Direct or broadcast message sent between sibling agents on `SwarmBlackboard`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentMailMessage {
    pub from_id: String,
    pub to_id: String,
    pub message: String,
    pub created_at: u64,
}

#[derive(Debug, Clone)]
struct TrackedMail {
    mail: AgentMailMessage,
    delivered_to: HashSet<String>,
}

#[derive(Debug, Default)]
struct BlackboardInner {
    entries: BTreeMap<String, BlackboardEntry>,
    mails: Vec<TrackedMail>,
}

/// Thread-safe shared blackboard and inter-agent mailbox allowing concurrent workers
/// to publish API contracts, ports, schemas, and point-to-point or broadcast notes in real time.
#[derive(Debug, Clone, Default)]
pub struct SwarmBlackboard {
    inner: Arc<RwLock<BlackboardInner>>,
}

impl SwarmBlackboard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Publishes or updates a shared key-value fact on the blackboard.
    pub fn publish(
        &self,
        key: impl Into<String>,
        value: impl Into<String>,
        author_id: impl Into<String>,
    ) -> BlackboardEntry {
        let entry = BlackboardEntry {
            key: key.into(),
            value: value.into(),
            author_id: author_id.into(),
            updated_at: now_epoch_ms(),
        };
        if let Ok(mut guard) = self.inner.write() {
            guard.entries.insert(entry.key.clone(), entry.clone());
        }
        entry
    }

    /// Retrieves a single entry by key if present.
    pub fn get(&self, key: &str) -> Option<BlackboardEntry> {
        let guard = self.inner.read().ok()?;
        guard.entries.get(key).cloned()
    }

    /// Lists all published blackboard entries in deterministic key order.
    pub fn list_entries(&self) -> Vec<BlackboardEntry> {
        let Ok(guard) = self.inner.read() else {
            return Vec::new();
        };
        guard.entries.values().cloned().collect()
    }

    /// Sends a message to a specific `to_id` or broadcasts to all sibling workers (`"*"` / `"all"`).
    pub fn send_mail(
        &self,
        from_id: impl Into<String>,
        to_id: impl Into<String>,
        message: impl Into<String>,
    ) -> AgentMailMessage {
        let mail = AgentMailMessage {
            from_id: from_id.into(),
            to_id: to_id.into(),
            message: message.into(),
            created_at: now_epoch_ms(),
        };
        if let Ok(mut guard) = self.inner.write() {
            guard.mails.push(TrackedMail {
                mail: mail.clone(),
                delivered_to: HashSet::new(),
            });
        }
        mail
    }

    /// Drains all unread messages addressed to `recipient_id` (including unread broadcasts not sent
    /// by `recipient_id`), marking them delivered for `recipient_id`.
    pub fn drain_mail_for(&self, recipient_id: &str) -> Vec<AgentMailMessage> {
        let target = recipient_id.trim();
        if target.is_empty() {
            return Vec::new();
        }
        let Ok(mut guard) = self.inner.write() else {
            return Vec::new();
        };

        let mut drained = Vec::new();
        for item in &mut guard.mails {
            if is_mail_for_recipient(&item.mail, target) && !item.delivered_to.contains(target) {
                item.delivered_to.insert(target.to_string());
                drained.push(item.mail.clone());
            }
        }
        drained
    }

    /// Returns the full chronological log of all inter-agent mail sent during the mission.
    pub fn list_all_mail(&self) -> Vec<AgentMailMessage> {
        let Ok(guard) = self.inner.read() else {
            return Vec::new();
        };
        guard.mails.iter().map(|t| t.mail.clone()).collect()
    }

    /// Formats shared blackboard contracts and any unread mail for `worker_id` into a concise
    /// prompt block without mutating unread delivery state.
    pub fn format_for_prompt(&self, worker_id: &str) -> String {
        let Ok(guard) = self.inner.read() else {
            return String::new();
        };

        let target = worker_id.trim();
        let unread: Vec<&AgentMailMessage> = guard
            .mails
            .iter()
            .filter(|item| {
                if target.is_empty() || target == "*" {
                    true
                } else {
                    is_mail_for_recipient(&item.mail, target) && !item.delivered_to.contains(target)
                }
            })
            .map(|item| &item.mail)
            .collect();

        if guard.entries.is_empty() && unread.is_empty() {
            return String::new();
        }

        let mut out = String::new();
        if !guard.entries.is_empty() {
            out.push_str("<swarm_blackboard>\n");
            for entry in guard.entries.values() {
                out.push_str(&format!(
                    "- [{}] (by `{}`): {}\n",
                    entry.key, entry.author_id, entry.value
                ));
            }
            out.push_str("</swarm_blackboard>\n");
        }

        if !unread.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str("<agent_mailbox>\n");
            for msg in unread {
                out.push_str(&format!(
                    "- From `{}` -> `{}`: {}\n",
                    msg.from_id, msg.to_id, msg.message
                ));
            }
            out.push_str("</agent_mailbox>\n");
        }

        out
    }
}

fn is_mail_for_recipient(mail: &AgentMailMessage, recipient_id: &str) -> bool {
    if mail.to_id == recipient_id {
        return true;
    }
    let is_broadcast = mail.to_id == "*" || mail.to_id.eq_ignore_ascii_case("all");
    is_broadcast && mail.from_id != recipient_id
}

/// Conflict record when `>= 2` write-enabled workers modify the same relative path divergently.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SwarmConflict {
    pub rel_path: String,
    pub worker_ids: Vec<String>,
}

/// Report returned when merging completed worker APFS CoW clones back into the workspace.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SwarmMergeReport {
    pub merged_files: Vec<String>,
    pub conflicts: Vec<SwarmConflict>,
    pub staging_clone_id: Option<String>,
}

/// Result and deterministic score of a single model candidate in a Best-of-N Arena run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ArenaCandidateResult {
    pub candidate_id: String,
    pub model: String,
    pub provider: String,
    pub clone_id: String,
    pub clone_path: String,
    pub ok: bool,
    pub summary: String,
    #[serde(default)]
    pub diffs: Vec<CloneFileDiff>,
    pub verify_passed: bool,
    #[serde(default)]
    pub verify_results: Vec<VerifyResult>,
    pub duration_ms: u64,
    pub score: i64,
}

/// Streaming lifecycle events emitted during Swarm DAG and Best-of-N Arena runs (`swarm_event`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SwarmEvent {
    #[serde(rename_all = "camelCase")]
    PlanReady {
        mission_id: String,
        goal: String,
        waves: Vec<Vec<SwarmTaskSpec>>,
        nodes: Vec<SwarmNodeState>,
    },
    #[serde(rename_all = "camelCase")]
    WaveStarted {
        mission_id: String,
        wave_index: usize,
        total_waves: usize,
        task_ids: Vec<String>,
    },
    #[serde(rename_all = "camelCase")]
    NodeUpdated {
        mission_id: String,
        node: SwarmNodeState,
    },
    #[serde(rename_all = "camelCase")]
    WorkerAction {
        mission_id: String,
        worker_id: String,
        turn: usize,
        action: String,
        detail: String,
        ok: bool,
    },
    #[serde(rename_all = "camelCase")]
    BlackboardPublished {
        mission_id: String,
        entry: BlackboardEntry,
    },
    #[serde(rename_all = "camelCase")]
    MailSent {
        mission_id: String,
        mail: AgentMailMessage,
    },
    #[serde(rename_all = "camelCase")]
    SwarmCompleted {
        mission_id: String,
        nodes: Vec<SwarmNodeState>,
        blackboard: Vec<BlackboardEntry>,
        mail: Vec<AgentMailMessage>,
        conflicts: Vec<SwarmConflict>,
        duration_ms: u64,
    },
    #[serde(rename_all = "camelCase")]
    ArenaStarted {
        arena_id: String,
        prompt: String,
        models: Vec<String>,
    },
    #[serde(rename_all = "camelCase")]
    ArenaCandidateUpdated {
        arena_id: String,
        candidate_id: String,
        model: String,
        status: String,
    },
    #[serde(rename_all = "camelCase")]
    ArenaCompleted {
        arena_id: String,
        candidates: Vec<ArenaCandidateResult>,
        winner_id: Option<String>,
        duration_ms: u64,
    },
}

/// Complete report returned at the end of `run_swarm_mission`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwarmMissionReport {
    pub mission_id: String,
    pub goal: String,
    pub waves: Vec<Vec<SwarmTaskSpec>>,
    pub nodes: Vec<SwarmNodeState>,
    pub blackboard: Vec<BlackboardEntry>,
    pub mail: Vec<AgentMailMessage>,
    pub conflicts: Vec<SwarmConflict>,
    pub duration_ms: u64,
}

/// Multi-provider Smart Router runtime dispatching turns across Gemini, Anthropic, OpenAI, and Local LLMs.
#[derive(Debug, Clone)]
pub struct SmartRouter {
    pub http: Client,
    pub gemini: GeminiClient,
    pub local_backend: String,
    pub local_endpoint: String,
    pub local_model: String,
}

impl Default for SmartRouter {
    fn default() -> Self {
        Self {
            http: Client::new(),
            gemini: GeminiClient::new(),
            local_backend: "gemini".to_string(),
            local_endpoint: "http://127.0.0.1:11434".to_string(),
            local_model: "qwen2.5-coder:7b".to_string(),
        }
    }
}

impl SmartRouter {
    pub fn new(http: Client, gemini: GeminiClient) -> Self {
        Self {
            http,
            gemini,
            ..Self::default()
        }
    }

    pub fn with_local(
        http: Client,
        gemini: GeminiClient,
        local_backend: impl Into<String>,
        local_endpoint: impl Into<String>,
        local_model: impl Into<String>,
    ) -> Self {
        Self {
            http,
            gemini,
            local_backend: local_backend.into(),
            local_endpoint: local_endpoint.into(),
            local_model: local_model.into(),
        }
    }

    pub fn decide_route(
        &self,
        kind: TaskKind,
        prompt_hint: &str,
        requested_model: Option<&str>,
        keys: &ProviderKeys,
    ) -> RouteDecision {
        route_task(
            kind,
            prompt_hint,
            requested_model,
            &self.local_backend,
            &self.local_model,
            keys,
        )
    }

    /// Executes a single chat completion turn against the routed provider.
    #[allow(clippy::too_many_arguments)]
    pub async fn complete_chat(
        &self,
        kind: TaskKind,
        prompt_hint: &str,
        requested_model: Option<&str>,
        system_prompt: Option<&str>,
        messages: &[ChatMessage],
        temperature: f32,
        max_tokens: Option<u32>,
        keys: &ProviderKeys,
    ) -> Result<(RouteDecision, String), String> {
        let decision = self.decide_route(kind, prompt_hint, requested_model, keys);

        let text = match decision.provider {
            ProviderKind::Anthropic => {
                let api_key = keys
                    .anthropic
                    .as_deref()
                    .map(str::trim)
                    .filter(|k| !k.is_empty())
                    .ok_or_else(|| {
                        format!(
                            "Anthropic API key is not configured for model '{}'.",
                            decision.model
                        )
                    })?;
                stream_anthropic_chat(
                    &self.http,
                    api_key,
                    &decision.model,
                    system_prompt,
                    messages,
                    temperature,
                    max_tokens,
                    decision.thinking_budget,
                    |_| {},
                )
                .await?
            }
            ProviderKind::OpenAi => {
                let api_key = keys
                    .openai
                    .as_deref()
                    .map(str::trim)
                    .filter(|k| !k.is_empty())
                    .ok_or_else(|| {
                        format!(
                            "OpenAI API key is not configured for model '{}'.",
                            decision.model
                        )
                    })?;
                stream_openai_chat(
                    &self.http,
                    api_key,
                    &decision.model,
                    system_prompt,
                    messages,
                    temperature,
                    max_tokens,
                    |_| {},
                )
                .await?
            }
            ProviderKind::LocalLlm => {
                self.complete_local_chat(&decision.model, system_prompt, messages, temperature, max_tokens)
                    .await?
            }
            ProviderKind::Gemini => {
                let api_key = keys
                    .gemini
                    .as_deref()
                    .map(str::trim)
                    .filter(|k| !k.is_empty())
                    .ok_or_else(|| {
                        format!(
                            "Gemini API key is not configured for model '{}'.",
                            decision.model
                        )
                    })?;
                let contents: Vec<Value> = messages
                    .iter()
                    .map(chat_message_to_gemini_content)
                    .collect();
                let out = self
                    .gemini
                    .stream_turn_with_options(
                        api_key,
                        &decision.model,
                        system_prompt,
                        &contents,
                        None,
                        temperature,
                        max_tokens,
                        GeminiTurnOptions {
                            thinking_budget: decision.thinking_budget,
                            cached_content: None,
                        },
                        |_, _| {},
                    )
                    .await?;
                out.text
            }
        };

        Ok((decision, text))
    }

    async fn complete_local_chat(
        &self,
        model: &str,
        system_prompt: Option<&str>,
        messages: &[ChatMessage],
        temperature: f32,
        max_tokens: Option<u32>,
    ) -> Result<String, String> {
        let base = self.local_endpoint.trim_end_matches('/');
        let url = if base.ends_with("/v1") {
            format!("{base}/chat/completions")
        } else {
            format!("{base}/v1/chat/completions")
        };

        let mut wire_msgs = Vec::new();
        if let Some(sys) = system_prompt.map(str::trim).filter(|s| !s.is_empty()) {
            wire_msgs.push(json!({ "role": "system", "content": sys }));
        }
        for msg in messages {
            let role = if msg.role == "model" || msg.role == "assistant" {
                "assistant"
            } else {
                "user"
            };
            wire_msgs.push(json!({ "role": role, "content": msg.content }));
        }

        let body = json!({
            "model": model,
            "messages": wire_msgs,
            "temperature": temperature,
            "max_tokens": max_tokens.unwrap_or(2048),
            "stream": false
        });

        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Local LLM request failed: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let err_txt = resp.text().await.unwrap_or_default();
            return Err(format!("Local LLM error ({status}): {err_txt}"));
        }
        let val: Value = resp
            .json()
            .await
            .map_err(|e| format!("Invalid Local LLM JSON response: {e}"))?;
        Ok(val
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string())
    }
}

pub fn provider_kind_label(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Gemini => "gemini",
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::OpenAi => "openai",
        ProviderKind::LocalLlm => "local",
    }
}

// ============================================================================
// 2. Topological Wave Scheduler (`compute_execution_waves`)
// ============================================================================

/// Validates unique task IDs, unknown dependencies, and dependency cycles, grouping independent
/// tasks into parallel execution waves (`Vec<Vec<SwarmTaskSpec>>`) via Kahn's algorithm.
pub fn compute_execution_waves(
    tasks: &[SwarmTaskSpec],
) -> Result<Vec<Vec<SwarmTaskSpec>>, String> {
    if tasks.is_empty() {
        return Ok(Vec::new());
    }

    let mut id_to_index: HashMap<&str, usize> = HashMap::with_capacity(tasks.len());
    for (idx, task) in tasks.iter().enumerate() {
        let id = task.id.trim();
        if id.is_empty() {
            return Err(format!("Task at index {idx} has an empty ID."));
        }
        if id_to_index.insert(id, idx).is_some() {
            return Err(format!("Duplicate task ID in swarm DAG: '{id}'."));
        }
    }

    let mut in_degree = vec![0usize; tasks.len()];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); tasks.len()];

    for (idx, task) in tasks.iter().enumerate() {
        let mut seen_deps: HashSet<&str> = HashSet::new();
        for raw_dep in &task.depends_on {
            let dep = raw_dep.trim();
            if dep.is_empty() {
                return Err(format!("Task '{}' contains an empty dependency ID.", task.id));
            }
            if dep == task.id.trim() {
                return Err(format!(
                    "Cycle detected in swarm DAG: task '{}' depends on itself.",
                    task.id
                ));
            }
            let Some(&dep_idx) = id_to_index.get(dep) else {
                return Err(format!(
                    "Task '{}' depends on unknown task '{}'.",
                    task.id, dep
                ));
            };
            if seen_deps.insert(dep) {
                in_degree[idx] += 1;
                dependents[dep_idx].push(idx);
            }
        }
    }

    let mut scheduled = vec![false; tasks.len()];
    let mut scheduled_count = 0usize;
    let mut waves: Vec<Vec<SwarmTaskSpec>> = Vec::new();

    while scheduled_count < tasks.len() {
        let ready_indices: Vec<usize> = (0..tasks.len())
            .filter(|&idx| !scheduled[idx] && in_degree[idx] == 0)
            .collect();

        if ready_indices.is_empty() {
            let cycle_ids: Vec<&str> = (0..tasks.len())
                .filter(|&idx| !scheduled[idx])
                .map(|idx| tasks[idx].id.as_str())
                .collect();
            return Err(format!(
                "Cycle detected in swarm DAG involving tasks: {}",
                cycle_ids.join(", ")
            ));
        }

        let mut wave = Vec::with_capacity(ready_indices.len());
        for &idx in &ready_indices {
            scheduled[idx] = true;
            scheduled_count += 1;
            wave.push(tasks[idx].clone());
        }

        for &idx in &ready_indices {
            for &dep_target in &dependents[idx] {
                in_degree[dep_target] = in_degree[dep_target].saturating_sub(1);
            }
        }

        waves.push(wave);
    }

    Ok(waves)
}

// ============================================================================
// 3. Goal Decomposer (`decompose_goal_into_dag`)
// ============================================================================

/// Deterministic 3-node fallback plan (`research` -> `implement` -> `verify`) used when model
/// JSON decomposition is unavailable or produces an invalid DAG.
pub fn fallback_goal_dag(goal: &str) -> Vec<SwarmTaskSpec> {
    let trimmed_goal = goal.trim();
    vec![
        SwarmTaskSpec {
            id: "research".to_string(),
            title: "Research Architecture & Contracts".to_string(),
            role: SwarmRole::Researcher,
            prompt: format!(
                "Inspect the repository structure and relevant files for the mission goal:\n\
                 \"{trimmed_goal}\"\n\n\
                 Publish discovered module paths, API contracts, types, and invariants to the shared blackboard via `publish_blackboard`, then `finish` with a concise architectural brief."
            ),
            depends_on: Vec::new(),
            write_enabled: false,
            model_override: None,
        },
        SwarmTaskSpec {
            id: "implement".to_string(),
            title: "Implement Changes in CoW Sandbox".to_string(),
            role: SwarmRole::Coder,
            prompt: format!(
                "Implement the required code changes to satisfy the mission goal:\n\
                 \"{trimmed_goal}\"\n\n\
                 Follow all upstream research findings and blackboard contracts. Validate edits cleanly and `finish` with a summary of modified files."
            ),
            depends_on: vec!["research".to_string()],
            write_enabled: true,
            model_override: None,
        },
        SwarmTaskSpec {
            id: "verify".to_string(),
            title: "Integration & Verification Review".to_string(),
            role: SwarmRole::Verifier,
            prompt: format!(
                "Review the upstream implementation summary, blackboard contracts, and verification results for:\n\
                 \"{trimmed_goal}\"\n\n\
                 Run read-only/test checks if needed and `finish` with an integration readiness report."
            ),
            depends_on: vec!["implement".to_string()],
            write_enabled: false,
            model_override: None,
        },
    ]
}

/// Parses and validates a 3–6 node `Vec<SwarmTaskSpec>` DAG from raw model text.
pub fn parse_decomposed_dag_json(raw: &str) -> Result<Vec<SwarmTaskSpec>, String> {
    let json_slice = extract_json_payload(raw)
        .ok_or_else(|| "No JSON array or object found in planner response.".to_string())?;
    let val: Value =
        serde_json::from_str(json_slice).map_err(|e| format!("Invalid planner JSON: {e}"))?;

    let tasks_val = if val.is_array() {
        val
    } else if let Some(arr) = val
        .get("tasks")
        .or_else(|| val.get("nodes"))
        .or_else(|| val.get("dag"))
    {
        arr.clone()
    } else {
        return Err("Planner JSON did not contain a task array.".to_string());
    };

    let mut tasks: Vec<SwarmTaskSpec> = serde_json::from_value(tasks_val)
        .map_err(|e| format!("Failed to deserialize SwarmTaskSpec list: {e}"))?;

    for task in &mut tasks {
        task.id = sanitize_task_id(&task.id);
        task.depends_on = task
            .depends_on
            .iter()
            .map(|d| sanitize_task_id(d))
            .filter(|d| !d.is_empty())
            .collect();
    }

    if !(3..=6).contains(&tasks.len()) {
        return Err(format!(
            "Expected 3..=6 swarm DAG nodes, got {}.",
            tasks.len()
        ));
    }

    compute_execution_waves(&tasks)?;
    Ok(tasks)
}

/// Uses `SmartRouter` / `GeminiClient` to decompose a high-level `goal` + `<repo_map>` summary
/// into a validated 3–6 node `Vec<SwarmTaskSpec>` DAG, falling back deterministically to
/// `research -> implement -> verify` if model JSON generation or DAG validation fails.
pub async fn decompose_goal_into_dag(
    goal: &str,
    repo_map: &str,
    model_override: Option<&str>,
    router: &SmartRouter,
    keys: &ProviderKeys,
) -> Vec<SwarmTaskSpec> {
    if !keys.has_gemini() && !keys.has_anthropic() && !keys.has_openai() {
        return fallback_goal_dag(goal);
    }

    let system_prompt = "\
You are Rursor's Multi-Agent Swarm Architect. Decompose the user's software engineering goal into \
a 3 to 6 node Directed Acyclic Graph (DAG) of parallelizable subtasks.\n\
Output ONLY valid JSON matching this exact array schema (no markdown commentary):\n\
[\n\
  {\n\
    \"id\": \"contracts\",\n\
    \"title\": \"Define Contracts & Research\",\n\
    \"role\": \"researcher\",\n\
    \"prompt\": \"Inspect relevant files and publish shared interfaces/contracts to the blackboard.\",\n\
    \"dependsOn\": [],\n\
    \"writeEnabled\": false,\n\
    \"modelOverride\": null\n\
  }\n\
]\n\
Rules:\n\
1. Create between 3 and 6 nodes total.\n\
2. Use unique lowercase snake_case IDs (`[a-z0-9_]+`).\n\
3. Roles may be `planner`, `researcher`, `coder`, `verifier`, `mediaProducer`, or a focused custom string.\n\
4. Assign `writeEnabled: true` ONLY to implementation/media workers that edit or create files, and ensure parallel `writeEnabled: true` workers in the same wave target NON-OVERLAPPING files so APFS CoW clones merge cleanly!\n\
5. Include a final `verifier` node (`writeEnabled: false`) depending on the implementation workers.";

    let trimmed_map: String = repo_map.chars().take(10_000).collect();
    let user_prompt = if trimmed_map.trim().is_empty() {
        format!("Mission Goal:\n{goal}\n\nReturn the 3-6 node JSON array now.")
    } else {
        format!(
            "<repo_map>\n{trimmed_map}\n</repo_map>\n\nMission Goal:\n{goal}\n\nReturn the 3-6 node JSON array now."
        )
    };

    let messages = vec![ChatMessage {
        role: "user".to_string(),
        content: user_prompt,
        images: Vec::new(),
    }];

    match router
        .complete_chat(
            TaskKind::AgentDeep,
            goal,
            model_override,
            Some(system_prompt),
            &messages,
            0.1,
            Some(3072),
            keys,
        )
        .await
    {
        Ok((_route, raw)) => parse_decomposed_dag_json(&raw).unwrap_or_else(|_| fallback_goal_dag(goal)),
        Err(_) => fallback_goal_dag(goal),
    }
}

// ============================================================================
// 4. Autonomous Multi-Turn Worker Loop (`execute_swarm_worker`)
// ============================================================================

/// Configuration options controlling an individual `execute_swarm_worker` invocation.
#[derive(Debug, Clone)]
pub struct SwarmWorkerOptions {
    pub mission_id: String,
    pub max_turns: usize,
    pub allow_network: bool,
    pub upstream_context: String,
}

impl Default for SwarmWorkerOptions {
    fn default() -> Self {
        Self {
            mission_id: "swarm".to_string(),
            max_turns: DEFAULT_WORKER_MAX_TURNS,
            allow_network: false,
            upstream_context: String::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorkerToolAction {
    #[serde(alias = "tool", alias = "name")]
    action: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    prefix: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    start_line: Option<usize>,
    #[serde(default)]
    end_line: Option<usize>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default, alias = "target", alias = "oldText", alias = "old_text")]
    search: Option<String>,
    #[serde(default, alias = "replacement", alias = "newText", alias = "new_text")]
    replace: Option<String>,
    #[serde(default)]
    replace_all: Option<bool>,
    #[serde(default, alias = "cmd")]
    command: Option<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    value: Option<String>,
    #[serde(default, alias = "toId", alias = "recipient")]
    to: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    summary: Option<String>,
}

/// Executes an autonomous multi-turn Swarm worker inside an isolated APFS Copy-on-Write clone
/// (when `task.write_enabled`) or read-only against `workspace_root` (when `!task.write_enabled`).
#[allow(clippy::too_many_arguments)]
pub async fn execute_swarm_worker(
    task: &SwarmTaskSpec,
    workspace_root: &Path,
    clone_mgr: &ApfsCloneManager,
    blackboard: &SwarmBlackboard,
    router: &SmartRouter,
    keys: &ProviderKeys,
    options: SwarmWorkerOptions,
    on_event: Arc<dyn Fn(SwarmEvent) + Send + Sync>,
) -> SwarmNodeState {
    let started = Instant::now();
    let max_turns = if options.max_turns == 0 {
        DEFAULT_WORKER_MAX_TURNS
    } else {
        options.max_turns
    };

    let mut state = SwarmNodeState::from_spec(task);
    state.status = SwarmNodeStatus::Running;
    on_event(SwarmEvent::NodeUpdated {
        mission_id: options.mission_id.clone(),
        node: state.clone(),
    });

    // 1. Provision isolated APFS Copy-on-Write sandbox clone if write-enabled.
    let work_root: PathBuf = if task.write_enabled {
        let ws = workspace_root.to_path_buf();
        let mgr = clone_mgr.clone();
        let label = format!("swarm_{}", task.id);
        let clone_res = tokio::task::spawn_blocking(move || mgr.create_clone(&ws, &label))
            .await
            .map_err(|e| format!("CoW clone task panicked: {e}"))
            .and_then(|r| r);

        match clone_res {
            Ok(meta) => {
                state.clone_id = Some(meta.clone_id);
                state.clone_path = Some(meta.clone_root.display().to_string());
                on_event(SwarmEvent::NodeUpdated {
                    mission_id: options.mission_id.clone(),
                    node: state.clone(),
                });
                meta.clone_root
            }
            Err(err) => {
                state.status = SwarmNodeStatus::Failed;
                state.error = Some(format!("Failed to create APFS CoW clone: {err}"));
                state.duration_ms = started.elapsed().as_millis() as u64;
                on_event(SwarmEvent::NodeUpdated {
                    mission_id: options.mission_id.clone(),
                    node: state.clone(),
                });
                return state;
            }
        }
    } else {
        workspace_root.to_path_buf()
    };

    // 2. Build worker system prompt and initial turn context.
    let system_prompt = build_worker_system_prompt(task);
    let initial_prompt = build_worker_initial_prompt(task, &work_root, blackboard, &options.upstream_context);
    let _ = blackboard.drain_mail_for(&task.id);

    let mut conversation = vec![ChatMessage {
        role: "user".to_string(),
        content: initial_prompt,
        images: Vec::new(),
    }];

    let task_kind = match task.role {
        SwarmRole::Planner | SwarmRole::Coder => TaskKind::AgentDeep,
        _ => TaskKind::AgentFast,
    };

    let mut finished_summary: Option<String> = None;
    let mut fatal_llm_error: Option<String> = None;

    for turn_idx in 1..=max_turns {
        // Inject any newly arrived inter-agent mail before the next model call.
        let new_mail = blackboard.drain_mail_for(&task.id);
        if !new_mail.is_empty() {
            if let Some(last_msg) = conversation.last_mut() {
                if last_msg.role == "user" {
                    last_msg.content.push_str("\n\n<new_agent_mail>\n");
                    for m in &new_mail {
                        last_msg
                            .content
                            .push_str(&format!("- From `{}`: {}\n", m.from_id, m.message));
                    }
                    last_msg.content.push_str("</new_agent_mail>");
                }
            }
        }

        let turn_res = router
            .complete_chat(
                task_kind,
                &task.prompt,
                task.model_override.as_deref(),
                Some(&system_prompt),
                &conversation,
                0.15,
                Some(4096),
                keys,
            )
            .await;

        let (_route, raw_reply) = match turn_res {
            Ok(pair) => pair,
            Err(err) => {
                fatal_llm_error = Some(err);
                break;
            }
        };

        conversation.push(ChatMessage {
            role: "assistant".to_string(),
            content: raw_reply.clone(),
            images: Vec::new(),
        });

        let actions = parse_worker_actions(&raw_reply);
        if actions.is_empty() {
            // Model replied with a direct natural-language summary rather than a JSON tool call.
            let clean = raw_reply.trim();
            if !clean.is_empty() {
                finished_summary = Some(truncate_text(clean, 2_000));
            }
            break;
        }

        let mut observations = Vec::new();
        let mut saw_finish = false;

        for action in actions {
            let action_name = action.action.trim().to_ascii_lowercase();
            if action_name == "finish" || action_name == "done" || action_name == "complete" {
                let sum = action
                    .summary
                    .or(action.message)
                    .unwrap_or_else(|| "Task completed.".to_string());
                finished_summary = Some(sum.clone());
                on_event(SwarmEvent::WorkerAction {
                    mission_id: options.mission_id.clone(),
                    worker_id: task.id.clone(),
                    turn: turn_idx,
                    action: "finish".to_string(),
                    detail: truncate_text(&sum, 200),
                    ok: true,
                });
                saw_finish = true;
                break;
            }

            let (ok, detail, tool_output) = execute_worker_tool_action(
                &action,
                task,
                &work_root,
                options.allow_network,
                blackboard,
                &options.mission_id,
                &on_event,
            )
            .await;

            on_event(SwarmEvent::WorkerAction {
                mission_id: options.mission_id.clone(),
                worker_id: task.id.clone(),
                turn: turn_idx,
                action: action_name.clone(),
                detail,
                ok,
            });

            observations.push(format!(
                "<tool_result action=\"{action_name}\" ok=\"{ok}\">\n{tool_output}\n</tool_result>"
            ));
        }

        if saw_finish {
            break;
        }

        let bb_snapshot = blackboard.format_for_prompt(&task.id);
        let mut next_user = observations.join("\n\n");
        if !bb_snapshot.is_empty() {
            next_user.push_str("\n\n");
            next_user.push_str(&bb_snapshot);
        }
        if turn_idx + 1 == max_turns {
            next_user.push_str(
                "\n\nFinal turn approaching: wrap up any pending work and emit `{\"action\": \"finish\", \"summary\": \"...\"}`.",
            );
        }

        conversation.push(ChatMessage {
            role: "user".to_string(),
            content: next_user,
            images: Vec::new(),
        });
    }

    if let Some(err) = fatal_llm_error {
        state.status = SwarmNodeStatus::Failed;
        state.error = Some(err.clone());
        state.summary = format!("Worker failed: {err}");
        if let Some(ref cid) = state.clone_id {
            let mgr = clone_mgr.clone();
            let cid_owned = cid.clone();
            state.modified_files = tokio::task::spawn_blocking(move || mgr.diff_clone(&cid_owned))
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or_default();
        }
        state.duration_ms = started.elapsed().as_millis() as u64;
        on_event(SwarmEvent::NodeUpdated {
            mission_id: options.mission_id,
            node: state.clone(),
        });
        return state;
    }

    state.summary = finished_summary.unwrap_or_else(|| {
        conversation
            .iter()
            .rev()
            .find(|m| m.role == "assistant")
            .map(|m| truncate_text(m.content.trim(), 1_200))
            .unwrap_or_else(|| "Worker completed turns.".to_string())
    });

    // 3. For write-enabled workers, run detected project verification commands & diff the CoW clone!
    if task.write_enabled {
        state.status = SwarmNodeStatus::Verifying;
        on_event(SwarmEvent::NodeUpdated {
            mission_id: options.mission_id.clone(),
            node: state.clone(),
        });

        let verify_cmds = detect_verify_commands(&work_root);
        let mut verify_results = Vec::with_capacity(verify_cmds.len());
        for vcmd in verify_cmds {
            match run_sandboxed_command_async(
                &vcmd.command,
                &work_root,
                PermissionProfile::SafeAuto,
                options.allow_network,
                DEFAULT_VERIFY_TIMEOUT_SECS,
            )
            .await
            {
                Ok(out) => {
                    verify_results.push(VerifyResult {
                        label: vcmd.label,
                        command: vcmd.command,
                        exit_code: out.exit_code,
                        output: truncate_text(&out.combined_output, MAX_TOOL_OUTPUT_CHARS),
                    });
                }
                Err(err) => {
                    verify_results.push(VerifyResult {
                        label: vcmd.label,
                        command: vcmd.command,
                        exit_code: 1,
                        output: err,
                    });
                }
            }
        }
        state.verify_results = verify_results;

        if let Some(ref cid) = state.clone_id {
            let mgr = clone_mgr.clone();
            let cid_owned = cid.clone();
            state.modified_files = tokio::task::spawn_blocking(move || mgr.diff_clone(&cid_owned))
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or_default();
        }

        if !state.verify_results.is_empty() && !all_verify_passed(&state.verify_results) {
            let feedback = format_verify_feedback(&state.verify_results);
            state.status = SwarmNodeStatus::Failed;
            state.error = Some(truncate_text(&feedback, 1_500));
        } else {
            state.status = SwarmNodeStatus::Completed;
        }
    } else {
        state.status = SwarmNodeStatus::Completed;
    }

    state.duration_ms = started.elapsed().as_millis() as u64;
    on_event(SwarmEvent::NodeUpdated {
        mission_id: options.mission_id,
        node: state.clone(),
    });
    state
}

fn build_worker_system_prompt(task: &SwarmTaskSpec) -> String {
    let write_policy = if task.write_enabled {
        "You have WRITE access inside an isolated macOS APFS Copy-on-Write workspace clone (`writeEnabled: true`). \
         Every `write_file` and `edit_file` is gated through Tree-sitter/JSON `validate_edit_syntax`—if rejected, repair syntax immediately."
    } else {
        "You are running in READ-ONLY mode (`writeEnabled: false`). Do NOT call `write_file` or `edit_file`; inspect files, run read/test commands, publish findings to the blackboard, and summarize."
    };

    format!(
        "You are an autonomous Rursor Swarm Worker.\n\
         Worker ID: `{id}` | Title: `{title}` | Role: `{role}`\n\
         {write_policy}\n\n\
         Respond on each turn with ONLY a JSON object (or `{{\"actions\": [...]}}` array) invoking one of these tools:\n\
         - `{{\"action\": \"list_files\", \"prefix\": \"optional/subdir\", \"limit\": 200}}`\n\
         - `{{\"action\": \"read_file\", \"path\": \"rel/path.rs\", \"startLine\": 1, \"endLine\": 200}}`\n\
         - `{{\"action\": \"write_file\", \"path\": \"rel/path.rs\", \"content\": \"full file content\"}}`\n\
         - `{{\"action\": \"edit_file\", \"path\": \"rel/path.rs\", \"search\": \"exact existing snippet\", \"replace\": \"new snippet\"}}`\n\
         - `{{\"action\": \"run_command\", \"command\": \"cargo check\", \"timeoutSecs\": 60}}`\n\
         - `{{\"action\": \"publish_blackboard\", \"key\": \"contract_name\", \"value\": \"details for sibling agents\"}}`\n\
         - `{{\"action\": \"send_mail\", \"to\": \"sibling_worker_id_or_*\", \"message\": \"coordination note\"}}`\n\
         - `{{\"action\": \"finish\", \"summary\": \"concise summary of findings or changes made\"}}`",
        id = task.id,
        title = task.title,
        role = task.role
    )
}

fn build_worker_initial_prompt(
    task: &SwarmTaskSpec,
    work_root: &Path,
    blackboard: &SwarmBlackboard,
    upstream_context: &str,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("## Assigned Task: {} (`{}`)\n", task.title, task.id));
    out.push_str(&format!("{}\n", task.prompt.trim()));

    let sample_files = list_confined_workspace_files(work_root, None, 120);
    if !sample_files.is_empty() {
        out.push_str("\n<workspace_files_preview>\n");
        for f in sample_files {
            out.push_str(&format!("{f}\n"));
        }
        out.push_str("</workspace_files_preview>\n");
    }

    if !upstream_context.trim().is_empty() {
        out.push_str("\n<upstream_dependencies>\n");
        out.push_str(upstream_context.trim());
        out.push_str("\n</upstream_dependencies>\n");
    }

    let bb = blackboard.format_for_prompt(&task.id);
    if !bb.is_empty() {
        out.push('\n');
        out.push_str(&bb);
    }

    out.push_str("\nEmit your next JSON tool action now.");
    out
}

fn parse_worker_actions(raw: &str) -> Vec<WorkerToolAction> {
    let Some(json_str) = extract_json_payload(raw) else {
        return Vec::new();
    };
    let Ok(val) = serde_json::from_str::<Value>(json_str) else {
        return Vec::new();
    };

    if val.is_array() {
        return serde_json::from_value::<Vec<WorkerToolAction>>(val).unwrap_or_default();
    }

    if let Some(arr) = val.get("actions") {
        if let Ok(list) = serde_json::from_value::<Vec<WorkerToolAction>>(arr.clone()) {
            return list;
        }
    }

    if let Ok(single) = serde_json::from_value::<WorkerToolAction>(val) {
        if !single.action.trim().is_empty() {
            return vec![single];
        }
    }

    Vec::new()
}

/// Executes a single worker tool action confined strictly inside `work_root`.
pub(crate) async fn execute_worker_tool_action(
    action: &WorkerToolAction,
    task: &SwarmTaskSpec,
    work_root: &Path,
    allow_network: bool,
    blackboard: &SwarmBlackboard,
    mission_id: &str,
    on_event: &Arc<dyn Fn(SwarmEvent) + Send + Sync>,
) -> (bool, String, String) {
    let name = action.action.trim().to_ascii_lowercase();
    match name.as_str() {
        "list_files" => {
            let limit = action.limit.unwrap_or(200).clamp(1, 1_000);
            let files = list_confined_workspace_files(work_root, action.prefix.as_deref(), limit);
            let detail = format!("listed {} file(s)", files.len());
            let body = if files.is_empty() {
                "(no matching files)".to_string()
            } else {
                files.join("\n")
            };
            (true, detail, body)
        }
        "read_file" => {
            let Some(raw_path) = action.path.as_deref() else {
                return (
                    false,
                    "missing path".to_string(),
                    "Error: `read_file` requires a `path` field.".to_string(),
                );
            };
            match read_confined_file(work_root, raw_path, action.start_line, action.end_line) {
                Ok((rel, text)) => (true, rel, text),
                Err(err) => (false, raw_path.to_string(), err),
            }
        }
        "write_file" => {
            if !task.write_enabled {
                return (
                    false,
                    "write blocked (read-only worker)".to_string(),
                    "Error: `write_file` is disabled because `writeEnabled` is false for this worker."
                        .to_string(),
                );
            }
            let Some(raw_path) = action.path.as_deref() else {
                return (
                    false,
                    "missing path".to_string(),
                    "Error: `write_file` requires `path` and `content`.".to_string(),
                );
            };
            let Some(content) = action.content.as_deref() else {
                return (
                    false,
                    raw_path.to_string(),
                    "Error: `write_file` requires a `content` string.".to_string(),
                );
            };
            match write_confined_file_gated(work_root, raw_path, content) {
                Ok(msg) => (true, raw_path.to_string(), msg),
                Err(err) => (false, raw_path.to_string(), err),
            }
        }
        "edit_file" => {
            if !task.write_enabled {
                return (
                    false,
                    "edit blocked (read-only worker)".to_string(),
                    "Error: `edit_file` is disabled because `writeEnabled` is false for this worker."
                        .to_string(),
                );
            }
            let Some(raw_path) = action.path.as_deref() else {
                return (
                    false,
                    "missing path".to_string(),
                    "Error: `edit_file` requires `path`, `search`, and `replace`.".to_string(),
                );
            };
            let Some(search) = action.search.as_deref() else {
                return (
                    false,
                    raw_path.to_string(),
                    "Error: `edit_file` requires a non-empty `search` string.".to_string(),
                );
            };
            let replace = action.replace.as_deref().unwrap_or("");
            let replace_all = action.replace_all.unwrap_or(false);
            match edit_confined_file_gated(work_root, raw_path, search, replace, replace_all) {
                Ok(msg) => (true, raw_path.to_string(), msg),
                Err(err) => (false, raw_path.to_string(), err),
            }
        }
        "run_command" => {
            let Some(cmd) = action.command.as_deref().map(str::trim).filter(|s| !s.is_empty())
            else {
                return (
                    false,
                    "missing command".to_string(),
                    "Error: `run_command` requires a non-empty `command`.".to_string(),
                );
            };
            let timeout = action
                .timeout_secs
                .unwrap_or(DEFAULT_COMMAND_TIMEOUT_SECS)
                .clamp(1, 300);
            match run_sandboxed_command_async(
                cmd,
                work_root,
                PermissionProfile::SafeAuto,
                allow_network,
                timeout,
            )
            .await
            {
                Ok(out) => {
                    let ok = out.exit_code == 0 && !out.timed_out;
                    let detail = format!("`{}` -> exit {}", truncate_text(cmd, 80), out.exit_code);
                    let rendered = format!(
                        "exit_code: {}\ntimed_out: {}\noutput:\n{}",
                        out.exit_code,
                        out.timed_out,
                        truncate_text(&out.combined_output, MAX_TOOL_OUTPUT_CHARS)
                    );
                    (ok, detail, rendered)
                }
                Err(err) => (false, truncate_text(cmd, 80), err),
            }
        }
        "publish_blackboard" => {
            let Some(key) = action.key.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
                return (
                    false,
                    "missing blackboard key".to_string(),
                    "Error: `publish_blackboard` requires `key` and `value`.".to_string(),
                );
            };
            let value = action
                .value
                .as_deref()
                .or(action.content.as_deref())
                .unwrap_or("");
            let entry = blackboard.publish(key, value, &task.id);
            on_event(SwarmEvent::BlackboardPublished {
                mission_id: mission_id.to_string(),
                entry: entry.clone(),
            });
            (
                true,
                format!("published `{key}`"),
                format!("Published blackboard key `{key}`."),
            )
        }
        "send_mail" => {
            let Some(to) = action.to.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
                return (
                    false,
                    "missing recipient".to_string(),
                    "Error: `send_mail` requires `to` and `message`.".to_string(),
                );
            };
            let msg = action
                .message
                .as_deref()
                .or(action.value.as_deref())
                .unwrap_or("");
            let mail = blackboard.send_mail(&task.id, to, msg);
            on_event(SwarmEvent::MailSent {
                mission_id: mission_id.to_string(),
                mail,
            });
            (
                true,
                format!("mail -> `{to}`"),
                format!("Sent mail to `{to}`."),
            )
        }
        other => (
            false,
            format!("unknown action `{other}`"),
            format!(
                "Unknown action `{other}`. Supported actions: list_files, read_file, write_file, edit_file, run_command, publish_blackboard, send_mail, finish."
            ),
        ),
    }
}

/// Executes a full multi-wave Swarm DAG mission using `compute_execution_waves` and `JoinSet`.
#[allow(clippy::too_many_arguments)]
pub async fn run_swarm_mission(
    goal: &str,
    tasks_override: Option<Vec<SwarmTaskSpec>>,
    repo_map: &str,
    max_parallel_workers: usize,
    max_turns: usize,
    allow_network: bool,
    model_override: Option<&str>,
    workspace_root: &Path,
    clone_mgr: &ApfsCloneManager,
    router: &SmartRouter,
    keys: &ProviderKeys,
    on_event: Arc<dyn Fn(SwarmEvent) + Send + Sync>,
) -> Result<SwarmMissionReport, String> {
    let started = Instant::now();
    let seq = NEXT_SWARM_SEQ.fetch_add(1, Ordering::Relaxed);
    let mission_id = format!("swarm_{}_{}", now_epoch_ms(), seq);

    let mut tasks = match tasks_override {
        Some(explicit) if !explicit.is_empty() => explicit,
        _ => decompose_goal_into_dag(goal, repo_map, model_override, router, keys).await,
    };

    if let Some(m) = model_override.map(str::trim).filter(|s| !s.is_empty()) {
        for t in &mut tasks {
            if t.model_override.is_none() {
                t.model_override = Some(m.to_string());
            }
        }
    }

    let waves = compute_execution_waves(&tasks)?;
    let task_order: Vec<String> = tasks.iter().map(|t| t.id.clone()).collect();
    let mut node_map: HashMap<String, SwarmNodeState> = tasks
        .iter()
        .map(|t| (t.id.clone(), SwarmNodeState::from_spec(t)))
        .collect();

    let ordered_snapshot = |map: &HashMap<String, SwarmNodeState>| -> Vec<SwarmNodeState> {
        task_order
            .iter()
            .filter_map(|id| map.get(id).cloned())
            .collect()
    };

    on_event(SwarmEvent::PlanReady {
        mission_id: mission_id.clone(),
        goal: goal.to_string(),
        waves: waves.clone(),
        nodes: ordered_snapshot(&node_map),
    });

    let blackboard = SwarmBlackboard::new();
    let concurrency = max_parallel_workers.max(1);
    let semaphore = Arc::new(Semaphore::new(concurrency));
    let total_waves = waves.len();

    for (wave_index, wave) in waves.iter().enumerate() {
        on_event(SwarmEvent::WaveStarted {
            mission_id: mission_id.clone(),
            wave_index,
            total_waves,
            task_ids: wave.iter().map(|t| t.id.clone()).collect(),
        });

        let mut join_set: JoinSet<SwarmNodeState> = JoinSet::new();

        for spec in wave {
            // Check if any upstream dependency failed or was blocked.
            let failed_dep = spec.depends_on.iter().find(|dep_id| {
                node_map
                    .get(*dep_id)
                    .map(|n| {
                        matches!(
                            n.status,
                            SwarmNodeStatus::Failed | SwarmNodeStatus::Blocked
                        )
                    })
                    .unwrap_or(true)
            });

            if let Some(dep_id) = failed_dep {
                let mut blocked = SwarmNodeState::from_spec(spec);
                blocked.status = SwarmNodeStatus::Blocked;
                blocked.error = Some(format!(
                    "Blocked because upstream dependency '{dep_id}' did not complete cleanly."
                ));
                blocked.summary = blocked.error.clone().unwrap_or_default();
                on_event(SwarmEvent::NodeUpdated {
                    mission_id: mission_id.clone(),
                    node: blocked.clone(),
                });
                node_map.insert(spec.id.clone(), blocked);
                continue;
            }

            let mut upstream_ctx = String::new();
            for dep_id in &spec.depends_on {
                if let Some(dep_node) = node_map.get(dep_id) {
                    upstream_ctx.push_str(&format!(
                        "### Upstream `{}` ({} — {:?})\n{}\n",
                        dep_node.id, dep_node.title, dep_node.status, dep_node.summary
                    ));
                    if !dep_node.modified_files.is_empty() {
                        let paths: Vec<&str> = dep_node
                            .modified_files
                            .iter()
                            .map(|d| d.rel_path.as_str())
                            .collect();
                        upstream_ctx.push_str(&format!("Modified files: {}\n", paths.join(", ")));
                    }
                    upstream_ctx.push('\n');
                }
            }

            let sem_clone = Arc::clone(&semaphore);
            let spec_owned = spec.clone();
            let ws_owned = workspace_root.to_path_buf();
            let mgr_owned = clone_mgr.clone();
            let bb_owned = blackboard.clone();
            let router_owned = router.clone();
            let keys_owned = keys.clone();
            let ev_owned = Arc::clone(&on_event);
            let worker_opts = SwarmWorkerOptions {
                mission_id: mission_id.clone(),
                max_turns,
                allow_network,
                upstream_context: upstream_ctx,
            };

            join_set.spawn(async move {
                let _permit = sem_clone.acquire_owned().await;
                execute_swarm_worker(
                    &spec_owned,
                    &ws_owned,
                    &mgr_owned,
                    &bb_owned,
                    &router_owned,
                    &keys_owned,
                    worker_opts,
                    ev_owned,
                )
                .await
            });
        }

        while let Some(joined) = join_set.join_next().await {
            if let Ok(node_state) = joined {
                node_map.insert(node_state.id.clone(), node_state);
            }
        }
    }

    let final_nodes = ordered_snapshot(&node_map);
    let conflicts = detect_clone_conflicts(&final_nodes, clone_mgr).unwrap_or_default();
    let bb_entries = blackboard.list_entries();
    let all_mail = blackboard.list_all_mail();
    let duration_ms = started.elapsed().as_millis() as u64;

    on_event(SwarmEvent::SwarmCompleted {
        mission_id: mission_id.clone(),
        nodes: final_nodes.clone(),
        blackboard: bb_entries.clone(),
        mail: all_mail.clone(),
        conflicts: conflicts.clone(),
        duration_ms,
    });

    Ok(SwarmMissionReport {
        mission_id,
        goal: goal.to_string(),
        waves,
        nodes: final_nodes,
        blackboard: bb_entries,
        mail: all_mail,
        conflicts,
        duration_ms,
    })
}

// ============================================================================
// 5. Non-Overlapping Multi-Clone Merger
// ============================================================================

struct WorkerCloneChanges {
    worker_id: String,
    clone_id: String,
    clone_root: PathBuf,
    rel_paths: BTreeSet<String>,
}

fn collect_completed_worker_changes(
    nodes: &[SwarmNodeState],
    clone_mgr: &ApfsCloneManager,
) -> Result<Vec<WorkerCloneChanges>, String> {
    let mut out = Vec::new();
    for node in nodes {
        if matches!(
            node.status,
            SwarmNodeStatus::Failed | SwarmNodeStatus::Blocked
        ) {
            continue;
        }
        let Some(ref clone_id) = node.clone_id else {
            continue;
        };
        let meta = clone_mgr.get_clone(clone_id)?;
        let diffs = clone_mgr.diff_clone(clone_id)?;
        let mut rel_paths = BTreeSet::new();
        for d in diffs {
            rel_paths.insert(d.rel_path);
        }
        for d in &node.modified_files {
            rel_paths.insert(d.rel_path.clone());
        }
        if !rel_paths.is_empty() {
            out.push(WorkerCloneChanges {
                worker_id: node.id.clone(),
                clone_id: clone_id.clone(),
                clone_root: meta.clone_root,
                rel_paths,
            });
        }
    }
    Ok(out)
}

/// Inspects `CloneFileDiff` across all completed write-enabled workers and flags any `rel_path`
/// touched by `>= 2` workers as a `SwarmConflict` unless all touching workers produced
/// byte-for-byte identical file states (including identical deletions).
pub fn detect_clone_conflicts(
    nodes: &[SwarmNodeState],
    clone_mgr: &ApfsCloneManager,
) -> Result<Vec<SwarmConflict>, String> {
    let worker_changes = collect_completed_worker_changes(nodes, clone_mgr)?;
    let mut touches: BTreeMap<String, Vec<(&str, &Path)>> = BTreeMap::new();

    for wc in &worker_changes {
        for rel_path in &wc.rel_paths {
            touches
                .entry(rel_path.clone())
                .or_default()
                .push((wc.worker_id.as_str(), wc.clone_root.as_path()));
        }
    }

    let mut conflicts = Vec::new();
    for (rel_path, owners) in touches {
        if owners.len() < 2 {
            continue;
        }

        let first_state = read_optional_file_bytes(&owners[0].1.join(&rel_path))?;
        let mut all_identical = true;
        for (_, clone_root) in &owners[1..] {
            let candidate_state = read_optional_file_bytes(&clone_root.join(&rel_path))?;
            if candidate_state != first_state {
                all_identical = false;
                break;
            }
        }

        if !all_identical {
            let mut worker_ids: Vec<String> =
                owners.iter().map(|(wid, _)| (*wid).to_string()).collect();
            worker_ids.dedup();
            conflicts.push(SwarmConflict {
                rel_path,
                worker_ids,
            });
        }
    }

    Ok(conflicts)
}

/// Merges all non-conflicting files from completed write-enabled worker CoW clones back into
/// `workspace_root` using `ApfsCloneManager::apply_clone_back`, leaving any conflicting files
/// untouched and reporting them in `SwarmMergeReport`.
pub fn merge_worker_clones_to_workspace(
    nodes: &[SwarmNodeState],
    workspace_root: &Path,
    clone_mgr: &ApfsCloneManager,
) -> Result<SwarmMergeReport, String> {
    if !workspace_root.is_dir() {
        return Err(format!(
            "Workspace root does not exist: {}",
            workspace_root.display()
        ));
    }

    let worker_changes = collect_completed_worker_changes(nodes, clone_mgr)?;
    let conflicts = detect_clone_conflicts(nodes, clone_mgr)?;
    let conflicted_paths: HashSet<&str> =
        conflicts.iter().map(|c| c.rel_path.as_str()).collect();

    let mut already_scheduled: HashSet<String> = HashSet::new();
    let mut merged_set: BTreeSet<String> = BTreeSet::new();

    for wc in &worker_changes {
        let mut selected: Vec<String> = Vec::new();
        for rel_path in &wc.rel_paths {
            if conflicted_paths.contains(rel_path.as_str()) {
                continue;
            }
            if already_scheduled.insert(rel_path.clone()) {
                selected.push(rel_path.clone());
            }
        }

        if !selected.is_empty() {
            let applied = clone_mgr.apply_clone_back(&wc.clone_id, Some(&selected))?;
            for p in applied {
                merged_set.insert(p);
            }
        }
    }

    Ok(SwarmMergeReport {
        merged_files: merged_set.into_iter().collect(),
        conflicts,
        staging_clone_id: None,
    })
}

/// Creates a clean APFS Copy-on-Write staging clone of `workspace_root` (`swarm_staging_*`) and
/// copies all non-conflicting worker changes into that staging clone so integration verification
/// can run before mutating the user's primary workspace.
pub fn merge_worker_clones_to_staging(
    nodes: &[SwarmNodeState],
    workspace_root: &Path,
    clone_mgr: &ApfsCloneManager,
) -> Result<SwarmMergeReport, String> {
    let staging = clone_mgr.create_clone(workspace_root, "swarm_staging")?;
    let worker_changes = collect_completed_worker_changes(nodes, clone_mgr)?;
    let conflicts = detect_clone_conflicts(nodes, clone_mgr)?;
    let conflicted_paths: HashSet<&str> =
        conflicts.iter().map(|c| c.rel_path.as_str()).collect();

    let mut already_scheduled: HashSet<String> = HashSet::new();
    let mut merged_set: BTreeSet<String> = BTreeSet::new();

    for wc in &worker_changes {
        for rel_path in &wc.rel_paths {
            if conflicted_paths.contains(rel_path.as_str()) {
                continue;
            }
            if !already_scheduled.insert(rel_path.clone()) {
                continue;
            }
            let src = wc.clone_root.join(rel_path);
            let dst = staging.clone_root.join(rel_path);
            if src.exists() {
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|e| format!("Failed creating staging parent dir: {e}"))?;
                }
                let bytes = fs::read(&src)
                    .map_err(|e| format!("Failed reading worker file '{rel_path}': {e}"))?;
                fs::write(&dst, bytes)
                    .map_err(|e| format!("Failed writing staging file '{rel_path}': {e}"))?;
            } else if dst.exists() {
                fs::remove_file(&dst)
                    .map_err(|e| format!("Failed deleting staging file '{rel_path}': {e}"))?;
            }
            merged_set.insert(rel_path.clone());
        }
    }

    Ok(SwarmMergeReport {
        merged_files: merged_set.into_iter().collect(),
        conflicts,
        staging_clone_id: Some(staging.clone_id),
    })
}

/// Destroys specified swarm CoW clones (or all `swarm_` / `arena_` CoW clones if `clone_ids` is empty).
pub fn cleanup_swarm_clones(clone_ids: &[String], clone_mgr: &ApfsCloneManager) -> Vec<String> {
    let targets: Vec<String> = if clone_ids.is_empty() {
        clone_mgr
            .list_clones()
            .into_iter()
            .filter(|m| {
                m.task_label.starts_with("swarm_") || m.task_label.starts_with("arena_")
            })
            .map(|m| m.clone_id)
            .collect()
    } else {
        clone_ids.to_vec()
    };

    let mut destroyed = Vec::new();
    for cid in targets {
        if clone_mgr.destroy_clone(&cid).unwrap_or(false) {
            destroyed.push(cid);
        }
    }
    destroyed
}

// ============================================================================
// 6. Best-of-N Model Arena (`run_model_arena`)
// ============================================================================

/// Computes the deterministic Best-of-N Arena candidate score:
/// - `verify_passed`: `+1000`
/// - `ok`: `+200`
/// - non-empty diff: `+100`
/// - bounded latency tie-breaker (`0..=99` points, 1 point per 100ms) so faster runs win ties
///   without ever inverting a `+100` quality tier.
pub fn score_arena_candidate(
    verify_passed: bool,
    ok: bool,
    has_non_empty_diff: bool,
    duration_ms: u64,
) -> i64 {
    let mut base: i64 = 0;
    if verify_passed {
        base += 1000;
    }
    if ok {
        base += 200;
    }
    if has_non_empty_diff {
        base += 100;
    }
    if base == 0 {
        return 0;
    }
    let latency_penalty = (duration_ms / 100).min(99) as i64;
    base - latency_penalty
}

/// Recomputes scores and sorts Arena candidates highest-score-first with deterministic
/// `duration_ms` ascending tie-breaking.
pub fn rank_arena_candidates(candidates: &mut [ArenaCandidateResult]) {
    for c in candidates.iter_mut() {
        c.score = score_arena_candidate(
            c.verify_passed,
            c.ok,
            !c.diffs.is_empty(),
            c.duration_ms,
        );
    }
    candidates.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.duration_ms.cmp(&b.duration_ms))
            .then_with(|| a.candidate_id.cmp(&b.candidate_id))
    });
}

/// Runs `execute_swarm_worker` concurrently across all `N` requested models in separate APFS
/// Copy-on-Write clones, verifies each candidate independently, and returns candidates sorted
/// highest-score-first.
pub async fn run_model_arena(
    prompt: &str,
    models: Vec<String>,
    workspace_root: &Path,
    clone_mgr: &ApfsCloneManager,
    router: &SmartRouter,
    keys: &ProviderKeys,
) -> Vec<ArenaCandidateResult> {
    run_model_arena_with_events(
        prompt,
        models,
        workspace_root,
        clone_mgr,
        router,
        keys,
        false,
        Arc::new(|_| {}),
    )
    .await
}

/// Event-streaming variant of `run_model_arena`.
#[allow(clippy::too_many_arguments)]
pub async fn run_model_arena_with_events(
    prompt: &str,
    models: Vec<String>,
    workspace_root: &Path,
    clone_mgr: &ApfsCloneManager,
    router: &SmartRouter,
    keys: &ProviderKeys,
    allow_network: bool,
    on_event: Arc<dyn Fn(SwarmEvent) + Send + Sync>,
) -> Vec<ArenaCandidateResult> {
    let started = Instant::now();
    let seq = NEXT_SWARM_SEQ.fetch_add(1, Ordering::Relaxed);
    let arena_id = format!("arena_{}_{}", now_epoch_ms(), seq);

    let cleaned_models: Vec<String> = models
        .into_iter()
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .collect();

    if cleaned_models.is_empty() {
        return Vec::new();
    }

    on_event(SwarmEvent::ArenaStarted {
        arena_id: arena_id.clone(),
        prompt: prompt.to_string(),
        models: cleaned_models.clone(),
    });

    let mut join_set: JoinSet<ArenaCandidateResult> = JoinSet::new();

    for (idx, model_name) in cleaned_models.into_iter().enumerate() {
        let candidate_id = format!("arena_{}_{}", idx + 1, sanitize_task_id(&model_name));
        let route = router.decide_route(TaskKind::AgentDeep, prompt, Some(&model_name), keys);
        let provider_str = provider_kind_label(route.provider).to_string();

        let spec = SwarmTaskSpec {
            id: candidate_id.clone(),
            title: format!("Arena Candidate ({model_name})"),
            role: SwarmRole::Coder,
            prompt: prompt.to_string(),
            depends_on: Vec::new(),
            write_enabled: true,
            model_override: Some(model_name.clone()),
        };

        let ws_owned = workspace_root.to_path_buf();
        let mgr_owned = clone_mgr.clone();
        let router_owned = router.clone();
        let keys_owned = keys.clone();
        let arena_id_owned = arena_id.clone();
        let ev_owned = Arc::clone(&on_event);

        join_set.spawn(async move {
            ev_owned(SwarmEvent::ArenaCandidateUpdated {
                arena_id: arena_id_owned.clone(),
                candidate_id: candidate_id.clone(),
                model: model_name.clone(),
                status: "running".to_string(),
            });

            let bb = SwarmBlackboard::new();
            let node = execute_swarm_worker(
                &spec,
                &ws_owned,
                &mgr_owned,
                &bb,
                &router_owned,
                &keys_owned,
                SwarmWorkerOptions {
                    mission_id: arena_id_owned.clone(),
                    max_turns: DEFAULT_WORKER_MAX_TURNS,
                    allow_network,
                    upstream_context: String::new(),
                },
                Arc::clone(&ev_owned),
            )
            .await;

            // `ok` tracks whether the worker finished turns without a fatal clone/provider error,
            // whereas `verify_passed` tracks whether all detected verification checks passed.
            let fatal_exec_err = node.clone_id.is_none()
                || node
                    .error
                    .as_deref()
                    .map(|e| {
                        e.contains("API key is not configured")
                            || e.contains("API error")
                            || e.contains("request failed")
                            || e.contains("Failed to create APFS CoW clone")
                    })
                    .unwrap_or(false);
            let ok = !fatal_exec_err && !node.summary.trim().is_empty();
            let verify_passed = ok && all_verify_passed(&node.verify_results);
            let score = score_arena_candidate(
                verify_passed,
                ok,
                !node.modified_files.is_empty(),
                node.duration_ms,
            );

            ev_owned(SwarmEvent::ArenaCandidateUpdated {
                arena_id: arena_id_owned,
                candidate_id: candidate_id.clone(),
                model: model_name.clone(),
                status: if verify_passed {
                    "completed".to_string()
                } else if ok {
                    "verify_failed".to_string()
                } else {
                    "failed".to_string()
                },
            });

            ArenaCandidateResult {
                candidate_id,
                model: model_name,
                provider: provider_str,
                clone_id: node.clone_id.unwrap_or_default(),
                clone_path: node.clone_path.unwrap_or_default(),
                ok,
                summary: node.summary,
                diffs: node.modified_files,
                verify_passed,
                verify_results: node.verify_results,
                duration_ms: node.duration_ms,
                score,
            }
        });
    }

    let mut results = Vec::new();
    while let Some(joined) = join_set.join_next().await {
        if let Ok(item) = joined {
            results.push(item);
        }
    }

    rank_arena_candidates(&mut results);
    let winner_id = results.first().map(|c| c.candidate_id.clone());

    on_event(SwarmEvent::ArenaCompleted {
        arena_id,
        candidates: results.clone(),
        winner_id,
        duration_ms: started.elapsed().as_millis() as u64,
    });

    results
}

// ============================================================================
// Internal Confinement, File Editing (Syntax-Gated) & JSON Parsing Helpers
// ============================================================================

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn sanitize_task_id(raw: &str) -> String {
    let mapped: String = raw
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = mapped.trim_matches('_');
    if trimmed.is_empty() {
        "worker".to_string()
    } else {
        trimmed.chars().take(40).collect()
    }
}

fn truncate_text(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let head: String = text.chars().take(max_chars).collect();
    format!("{head}\n...(truncated {} chars)...", count - max_chars)
}

fn read_optional_file_bytes(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_file() || meta.file_type().is_symlink() {
                let bytes = fs::read(path)
                    .map_err(|e| format!("Failed reading '{}': {e}", path.display()))?;
                Ok(Some(bytes))
            } else {
                Ok(None)
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("Failed stating '{}': {e}", path.display())),
    }
}

fn resolve_confined_worker_path(
    work_root: &Path,
    raw_rel: &str,
    forbid_git: bool,
) -> Result<(String, PathBuf), String> {
    let trimmed = raw_rel.trim();
    if trimmed.is_empty() || trimmed.contains('\0') {
        return Err("Invalid empty or NUL-containing path.".to_string());
    }
    let p = Path::new(trimmed);
    if p.is_absolute() || trimmed.starts_with('/') || trimmed.starts_with('\\') {
        return Err(format!(
            "Path '{raw_rel}' must be relative to the workspace root."
        ));
    }

    let canonical_root = work_root
        .canonicalize()
        .map_err(|e| format!("Failed to canonicalize work_root: {e}"))?;

    let mut clean_parts: Vec<String> = Vec::new();
    for comp in p.components() {
        match comp {
            Component::Normal(seg) => {
                let s = seg.to_string_lossy();
                if s.is_empty() || s.contains('\\') {
                    return Err(format!("Invalid path segment in '{raw_rel}'."));
                }
                clean_parts.push(s.to_string());
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!(
                    "Path '{raw_rel}' escapes workspace confinement ('..' is forbidden)."
                ));
            }
        }
    }

    if clean_parts.is_empty() {
        return Err(format!("Path '{raw_rel}' resolves to root directory."));
    }
    if forbid_git && clean_parts.iter().any(|c| c == ".git") {
        return Err(format!("Refusing to mutate protected .git path '{raw_rel}'."));
    }

    let mut cur = canonical_root.clone();
    for part in &clean_parts[..clean_parts.len() - 1] {
        cur.push(part);
        if let Ok(meta) = fs::symlink_metadata(&cur) {
            if meta.file_type().is_symlink() {
                return Err(format!(
                    "Refusing to traverse symlink directory '{}'.",
                    cur.display()
                ));
            }
            let resolved = cur
                .canonicalize()
                .map_err(|e| format!("Failed resolving '{}': {e}", cur.display()))?;
            if !resolved.starts_with(&canonical_root) {
                return Err(format!(
                    "Directory '{}' escapes workspace root.",
                    cur.display()
                ));
            }
        }
    }

    let rel_norm = clean_parts.join("/");
    let target = canonical_root.join(&rel_norm);
    if let Ok(meta) = fs::symlink_metadata(&target) {
        if meta.file_type().is_symlink() {
            let resolved = target
                .canonicalize()
                .map_err(|e| format!("Broken symlink '{rel_norm}': {e}"))?;
            if !resolved.starts_with(&canonical_root) {
                return Err(format!("Symlink '{rel_norm}' escapes workspace root."));
            }
        }
    }

    Ok((rel_norm, target))
}

fn read_confined_file(
    work_root: &Path,
    raw_path: &str,
    start_line: Option<usize>,
    end_line: Option<usize>,
) -> Result<(String, String), String> {
    let (rel_norm, abs_path) = resolve_confined_worker_path(work_root, raw_path, false)?;
    let content =
        fs::read_to_string(&abs_path).map_err(|e| format!("Cannot read '{rel_norm}': {e}"))?;
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return Ok((rel_norm, "(empty file)".to_string()));
    }

    let start = start_line.unwrap_or(1).max(1);
    let default_end = (start + MAX_TOOL_READ_LINES - 1).min(lines.len());
    let end = end_line
        .unwrap_or(default_end)
        .min(lines.len())
        .min(start + MAX_TOOL_READ_LINES - 1);
    if start > end {
        return Ok((rel_norm, String::new()));
    }

    let mut out = String::new();
    for idx in (start - 1)..end {
        out.push_str(&format!("{}: {}\n", idx + 1, lines[idx]));
    }
    Ok((rel_norm, out))
}

pub(crate) fn write_confined_file_gated(
    work_root: &Path,
    raw_path: &str,
    new_content: &str,
) -> Result<String, String> {
    let (rel_norm, abs_path) = resolve_confined_worker_path(work_root, raw_path, true)?;
    let before = fs::read_to_string(&abs_path).unwrap_or_default();

    let gate = validate_edit_syntax(&rel_norm, &before, new_content);
    if !gate.valid {
        return Err(gate.message);
    }

    if let Some(parent) = abs_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed creating parent directory for '{rel_norm}': {e}"))?;
    }
    fs::write(&abs_path, new_content)
        .map_err(|e| format!("Failed writing '{rel_norm}': {e}"))?;

    Ok(format!(
        "Wrote '{}' ({} bytes, syntax gate valid).",
        rel_norm,
        new_content.len()
    ))
}

pub(crate) fn edit_confined_file_gated(
    work_root: &Path,
    raw_path: &str,
    search: &str,
    replace: &str,
    replace_all: bool,
) -> Result<String, String> {
    if search.is_empty() {
        return Err("Error: `search` string in `edit_file` must not be empty.".to_string());
    }
    let (rel_norm, abs_path) = resolve_confined_worker_path(work_root, raw_path, true)?;
    let before =
        fs::read_to_string(&abs_path).map_err(|e| format!("Cannot read '{rel_norm}': {e}"))?;

    let matches_count = before.matches(search).count();
    if matches_count == 0 {
        return Err(format!(
            "Exact `search` snippet was not found in '{rel_norm}'. Call `read_file` to inspect exact lines and whitespace."
        ));
    }
    if matches_count > 1 && !replace_all {
        return Err(format!(
            "`search` snippet matched {matches_count} locations in '{rel_norm}'. Include more surrounding context lines to make it unique, or set `replaceAll: true`."
        ));
    }

    let after = if replace_all {
        before.replace(search, replace)
    } else {
        before.replacen(search, replace, 1)
    };

    let gate = validate_edit_syntax(&rel_norm, &before, &after);
    if !gate.valid {
        return Err(gate.message);
    }

    fs::write(&abs_path, &after)
        .map_err(|e| format!("Failed writing edited '{rel_norm}': {e}"))?;

    Ok(format!(
        "Edited '{rel_norm}' ({matches_count} replacement(s), syntax gate valid)."
    ))
}

fn list_confined_workspace_files(
    work_root: &Path,
    prefix: Option<&str>,
    limit: usize,
) -> Vec<String> {
    let Ok(canonical_root) = work_root.canonicalize() else {
        return Vec::new();
    };
    let norm_prefix = prefix
        .map(|p| p.trim().trim_start_matches("./").trim_end_matches('/').to_string())
        .filter(|p| !p.is_empty());

    let mut out = Vec::new();
    walk_workspace_files(&canonical_root, &canonical_root, &mut out);
    out.sort();

    if let Some(pref) = norm_prefix {
        let pref_slash = format!("{pref}/");
        out.retain(|p| p == &pref || p.starts_with(&pref_slash));
    }
    out.truncate(limit);
    out
}

fn walk_workspace_files(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() >= 4_000 {
            return;
        }
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let path = entry.path();

        if ft.is_dir() {
            if IGNORED_WALK_DIRS.contains(&name_str.as_ref()) {
                continue;
            }
            walk_workspace_files(root, &path, out);
        } else if ft.is_file() {
            if let Ok(rel) = path.strip_prefix(root) {
                let rel_str = rel
                    .components()
                    .filter_map(|c| match c {
                        Component::Normal(s) => Some(s.to_string_lossy().to_string()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("/");
                if !rel_str.is_empty() {
                    out.push(rel_str);
                }
            }
        }
    }
}

/// Extracts a fenced ````json ... ```` payload or the outermost balanced JSON array/object from `raw`.
fn extract_json_payload(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(fence_idx) = trimmed.find("```") {
        let after_open = &trimmed[fence_idx + 3..];
        if let Some(newline_pos) = after_open.find('\n') {
            let body_start = &after_open[newline_pos + 1..];
            if let Some(close_idx) = body_start.find("```") {
                let candidate = body_start[..close_idx].trim();
                if candidate.starts_with('{') || candidate.starts_with('[') {
                    return Some(candidate);
                }
            }
        }
    }

    let obj_pos = trimmed.find('{');
    let arr_pos = trimmed.find('[');
    let (start_idx, open_ch, close_ch) = match (obj_pos, arr_pos) {
        (Some(o), Some(a)) if o < a => (o, '{', '}'),
        (Some(_), Some(a)) => (a, '[', ']'),
        (Some(o), None) => (o, '{', '}'),
        (None, Some(a)) => (a, '[', ']'),
        (None, None) => return None,
    };

    let slice = &trimmed[start_idx..];
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;

    for (byte_idx, ch) in slice.char_indices() {
        if in_str {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            c if c == open_ch => depth += 1,
            c if c == close_ch => {
                depth -= 1;
                if depth == 0 {
                    return Some(&slice[..=byte_idx]);
                }
            }
            _ => {}
        }
    }

    None
}

// ============================================================================
// 7. Unit Tests (`swarm::tests`)
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_temp_dir(prefix: &str) -> PathBuf {
        let seq = NEXT_SWARM_SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("{prefix}_{nanos}_{seq}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_spec(id: &str, role: SwarmRole, deps: &[&str], write_enabled: bool) -> SwarmTaskSpec {
        SwarmTaskSpec {
            id: id.to_string(),
            title: format!("Task {id}"),
            role,
            prompt: format!("Execute {id}"),
            depends_on: deps.iter().map(|d| (*d).to_string()).collect(),
            write_enabled,
            model_override: None,
        }
    }

    #[test]
    fn computes_diamond_dag_execution_waves() {
        let tasks = vec![
            sample_spec("plan", SwarmRole::Planner, &[], false),
            sample_spec("backend_api", SwarmRole::Coder, &["plan"], true),
            sample_spec("frontend_ui", SwarmRole::Coder, &["plan"], true),
            sample_spec(
                "integration_verify",
                SwarmRole::Verifier,
                &["backend_api", "frontend_ui"],
                false,
            ),
        ];

        let waves = compute_execution_waves(&tasks).expect("diamond DAG should succeed");
        assert_eq!(waves.len(), 3);
        assert_eq!(waves[0].iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), vec!["plan"]);
        assert_eq!(
            waves[1].iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            vec!["backend_api", "frontend_ui"]
        );
        assert_eq!(
            waves[2].iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            vec!["integration_verify"]
        );
    }

    #[test]
    fn rejects_cyclic_dag_and_self_dependency() {
        let cyclic = vec![
            sample_spec("alpha", SwarmRole::Coder, &["gamma"], true),
            sample_spec("beta", SwarmRole::Coder, &["alpha"], true),
            sample_spec("gamma", SwarmRole::Verifier, &["beta"], false),
        ];
        let err = compute_execution_waves(&cyclic).unwrap_err();
        assert!(err.to_lowercase().contains("cycle"), "unexpected err: {err}");

        let self_dep = vec![sample_spec("solo", SwarmRole::Coder, &["solo"], true)];
        let self_err = compute_execution_waves(&self_dep).unwrap_err();
        assert!(self_err.to_lowercase().contains("cycle"));
    }

    #[test]
    fn rejects_missing_dependency_and_duplicate_task_ids() {
        let missing_dep = vec![
            sample_spec("research", SwarmRole::Researcher, &[], false),
            sample_spec("coder", SwarmRole::Coder, &["non_existent_node"], true),
        ];
        let err = compute_execution_waves(&missing_dep).unwrap_err();
        assert!(err.contains("non_existent_node"), "unexpected err: {err}");

        let dup_ids = vec![
            sample_spec("dup_worker", SwarmRole::Researcher, &[], false),
            sample_spec("dup_worker", SwarmRole::Coder, &[], true),
        ];
        let dup_err = compute_execution_waves(&dup_ids).unwrap_err();
        assert!(dup_err.contains("dup_worker"), "unexpected err: {dup_err}");
    }

    #[test]
    fn blackboard_publishes_contracts_and_delivers_direct_and_broadcast_mail() {
        let bb = SwarmBlackboard::new();

        let published = bb.publish("auth_port", "4180", "researcher");
        assert_eq!(published.key, "auth_port");
        assert_eq!(published.value, "4180");
        assert_eq!(bb.get("auth_port").unwrap().author_id, "researcher");

        bb.publish("api_schema", "POST /v1/session -> { token }", "planner");
        assert_eq!(bb.list_entries().len(), 2);

        bb.send_mail("researcher", "coder_api", "Bind server to 127.0.0.1:4180");
        bb.send_mail("planner", "*", "Freeze schema v1 before editing");

        // `format_for_prompt` previews both blackboard entries and unread mail without consuming mail.
        let prompt_block = bb.format_for_prompt("coder_api");
        assert!(prompt_block.contains("auth_port"));
        assert!(prompt_block.contains("api_schema"));
        assert!(prompt_block.contains("Bind server to 127.0.0.1:4180"));
        assert!(prompt_block.contains("Freeze schema v1 before editing"));

        // `coder_api` drains both its direct message and the broadcast once.
        let mail_api = bb.drain_mail_for("coder_api");
        assert_eq!(mail_api.len(), 2);
        assert!(bb.drain_mail_for("coder_api").is_empty());

        // `coder_ui` receives only the broadcast (not `coder_api`'s direct mail), and `planner`
        // does not receive its own broadcast.
        let mail_ui = bb.drain_mail_for("coder_ui");
        assert_eq!(mail_ui.len(), 1);
        assert_eq!(mail_ui[0].message, "Freeze schema v1 before editing");
        assert!(bb.drain_mail_for("planner").is_empty());
    }

    #[test]
    fn disjoint_worker_clones_merge_cleanly_to_workspace() {
        let base = make_temp_dir("rursor_swarm_merge_clean");
        let ws = base.join("workspace");
        let clones_dir = base.join("clones");
        fs::create_dir_all(ws.join("src")).unwrap();
        fs::write(ws.join("src/api.rs"), "pub fn port() -> u16 { 3000 }\n").unwrap();
        fs::write(ws.join("src/ui.ts"), "export const title = 'Old';\n").unwrap();

        let clone_mgr = ApfsCloneManager::with_base_dir(&clones_dir);
        let c1 = clone_mgr.create_clone(&ws, "swarm_w1").unwrap();
        let c2 = clone_mgr.create_clone(&ws, "swarm_w2").unwrap();

        fs::write(c1.clone_root.join("src/api.rs"), "pub fn port() -> u16 { 8080 }\n").unwrap();
        fs::write(c2.clone_root.join("src/ui.ts"), "export const title = 'Mission Control';\n")
            .unwrap();
        fs::write(c2.clone_root.join("src/theme.ts"), "export const accent = '#7C5CFF';\n")
            .unwrap();

        let nodes = vec![
            SwarmNodeState {
                id: "w1".to_string(),
                title: "API Worker".to_string(),
                role: SwarmRole::Coder,
                depends_on: vec![],
                status: SwarmNodeStatus::Completed,
                clone_id: Some(c1.clone_id.clone()),
                clone_path: Some(c1.clone_root.display().to_string()),
                summary: "Updated port".to_string(),
                modified_files: clone_mgr.diff_clone(&c1.clone_id).unwrap(),
                verify_results: vec![],
                duration_ms: 120,
                error: None,
            },
            SwarmNodeState {
                id: "w2".to_string(),
                title: "UI Worker".to_string(),
                role: SwarmRole::Coder,
                depends_on: vec![],
                status: SwarmNodeStatus::Completed,
                clone_id: Some(c2.clone_id.clone()),
                clone_path: Some(c2.clone_root.display().to_string()),
                summary: "Updated UI & added theme".to_string(),
                modified_files: clone_mgr.diff_clone(&c2.clone_id).unwrap(),
                verify_results: vec![],
                duration_ms: 150,
                error: None,
            },
        ];

        let conflicts = detect_clone_conflicts(&nodes, &clone_mgr).unwrap();
        assert!(conflicts.is_empty());

        let report = merge_worker_clones_to_workspace(&nodes, &ws, &clone_mgr).unwrap();
        assert!(report.conflicts.is_empty());
        assert_eq!(
            report.merged_files,
            vec![
                "src/api.rs".to_string(),
                "src/theme.ts".to_string(),
                "src/ui.ts".to_string()
            ]
        );
        assert_eq!(
            fs::read_to_string(ws.join("src/api.rs")).unwrap(),
            "pub fn port() -> u16 { 8080 }\n"
        );
        assert_eq!(
            fs::read_to_string(ws.join("src/ui.ts")).unwrap(),
            "export const title = 'Mission Control';\n"
        );
        assert_eq!(
            fs::read_to_string(ws.join("src/theme.ts")).unwrap(),
            "export const accent = '#7C5CFF';\n"
        );

        let cleaned = cleanup_swarm_clones(&[], &clone_mgr);
        assert_eq!(cleaned.len(), 2);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn overlapping_divergent_worker_edits_flagged_as_conflict_and_skipped() {
        let base = make_temp_dir("rursor_swarm_conflict");
        let ws = base.join("workspace");
        let clones_dir = base.join("clones");
        fs::create_dir_all(ws.join("src")).unwrap();
        fs::write(ws.join("src/shared.rs"), "pub const VERSION: u32 = 1;\n").unwrap();
        fs::write(ws.join("src/safe.rs"), "pub fn ok() -> bool { false }\n").unwrap();

        let clone_mgr = ApfsCloneManager::with_base_dir(&clones_dir);
        let c1 = clone_mgr.create_clone(&ws, "swarm_w1").unwrap();
        let c2 = clone_mgr.create_clone(&ws, "swarm_w2").unwrap();

        fs::write(c1.clone_root.join("src/shared.rs"), "pub const VERSION: u32 = 2;\n").unwrap();
        fs::write(c1.clone_root.join("src/safe.rs"), "pub fn ok() -> bool { true }\n").unwrap();
        fs::write(c2.clone_root.join("src/shared.rs"), "pub const VERSION: u32 = 99;\n").unwrap();

        let nodes = vec![
            SwarmNodeState {
                id: "worker_a".to_string(),
                title: "Worker A".to_string(),
                role: SwarmRole::Coder,
                depends_on: vec![],
                status: SwarmNodeStatus::Completed,
                clone_id: Some(c1.clone_id.clone()),
                clone_path: Some(c1.clone_root.display().to_string()),
                summary: String::new(),
                modified_files: clone_mgr.diff_clone(&c1.clone_id).unwrap(),
                verify_results: vec![],
                duration_ms: 100,
                error: None,
            },
            SwarmNodeState {
                id: "worker_b".to_string(),
                title: "Worker B".to_string(),
                role: SwarmRole::Coder,
                depends_on: vec![],
                status: SwarmNodeStatus::Completed,
                clone_id: Some(c2.clone_id.clone()),
                clone_path: Some(c2.clone_root.display().to_string()),
                summary: String::new(),
                modified_files: clone_mgr.diff_clone(&c2.clone_id).unwrap(),
                verify_results: vec![],
                duration_ms: 110,
                error: None,
            },
        ];

        let report = merge_worker_clones_to_workspace(&nodes, &ws, &clone_mgr).unwrap();
        assert_eq!(report.merged_files, vec!["src/safe.rs".to_string()]);
        assert_eq!(report.conflicts.len(), 1);
        assert_eq!(report.conflicts[0].rel_path, "src/shared.rs");
        assert_eq!(
            report.conflicts[0].worker_ids,
            vec!["worker_a".to_string(), "worker_b".to_string()]
        );

        // Non-conflicting file was applied; conflicting file in workspace remained untouched!
        assert_eq!(
            fs::read_to_string(ws.join("src/safe.rs")).unwrap(),
            "pub fn ok() -> bool { true }\n"
        );
        assert_eq!(
            fs::read_to_string(ws.join("src/shared.rs")).unwrap(),
            "pub const VERSION: u32 = 1;\n"
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn identical_overlapping_worker_edits_allowed_without_conflict() {
        let base = make_temp_dir("rursor_swarm_identical");
        let ws = base.join("workspace");
        let clones_dir = base.join("clones");
        fs::create_dir_all(ws.join("src")).unwrap();
        fs::write(ws.join("src/contract.rs"), "pub struct Token;\n").unwrap();

        let clone_mgr = ApfsCloneManager::with_base_dir(&clones_dir);
        let c1 = clone_mgr.create_clone(&ws, "swarm_w1").unwrap();
        let c2 = clone_mgr.create_clone(&ws, "swarm_w2").unwrap();

        let agreed = "pub struct Token(pub String);\n";
        fs::write(c1.clone_root.join("src/contract.rs"), agreed).unwrap();
        fs::write(c2.clone_root.join("src/contract.rs"), agreed).unwrap();

        let nodes = vec![
            SwarmNodeState {
                id: "w1".to_string(),
                title: "W1".to_string(),
                role: SwarmRole::Coder,
                depends_on: vec![],
                status: SwarmNodeStatus::Completed,
                clone_id: Some(c1.clone_id.clone()),
                clone_path: Some(c1.clone_root.display().to_string()),
                summary: String::new(),
                modified_files: clone_mgr.diff_clone(&c1.clone_id).unwrap(),
                verify_results: vec![],
                duration_ms: 90,
                error: None,
            },
            SwarmNodeState {
                id: "w2".to_string(),
                title: "W2".to_string(),
                role: SwarmRole::Coder,
                depends_on: vec![],
                status: SwarmNodeStatus::Completed,
                clone_id: Some(c2.clone_id.clone()),
                clone_path: Some(c2.clone_root.display().to_string()),
                summary: String::new(),
                modified_files: clone_mgr.diff_clone(&c2.clone_id).unwrap(),
                verify_results: vec![],
                duration_ms: 95,
                error: None,
            },
        ];

        let report = merge_worker_clones_to_workspace(&nodes, &ws, &clone_mgr).unwrap();
        assert!(report.conflicts.is_empty());
        assert_eq!(report.merged_files, vec!["src/contract.rs".to_string()]);
        assert_eq!(fs::read_to_string(ws.join("src/contract.rs")).unwrap(), agreed);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn ranks_arena_candidates_deterministically() {
        let sample_diff = CloneFileDiff {
            rel_path: "src/lib.rs".to_string(),
            status: "modified".to_string(),
            added_lines: 12,
            removed_lines: 3,
        };

        let mut candidates = vec![
            ArenaCandidateResult {
                candidate_id: "c_failed_api".to_string(),
                model: "gpt-4.1".to_string(),
                provider: "openai".to_string(),
                clone_id: "cow_1".to_string(),
                clone_path: "/tmp/cow_1".to_string(),
                ok: false,
                summary: "API key missing".to_string(),
                diffs: vec![],
                verify_passed: false,
                verify_results: vec![],
                duration_ms: 50,
                score: 0,
            },
            ArenaCandidateResult {
                candidate_id: "c_slow_pass".to_string(),
                model: "claude-sonnet-4-20250514".to_string(),
                provider: "anthropic".to_string(),
                clone_id: "cow_2".to_string(),
                clone_path: "/tmp/cow_2".to_string(),
                ok: true,
                summary: "All tests pass".to_string(),
                diffs: vec![sample_diff.clone()],
                verify_passed: true,
                verify_results: vec![],
                duration_ms: 4_500,
                score: 0,
            },
            ArenaCandidateResult {
                candidate_id: "c_ok_but_verify_fail".to_string(),
                model: "gemini-3.8-flash".to_string(),
                provider: "gemini".to_string(),
                clone_id: "cow_3".to_string(),
                clone_path: "/tmp/cow_3".to_string(),
                ok: true,
                summary: "Edited file, 1 test failed".to_string(),
                diffs: vec![sample_diff.clone()],
                verify_passed: false,
                verify_results: vec![],
                duration_ms: 800,
                score: 0,
            },
            ArenaCandidateResult {
                candidate_id: "c_fast_pass".to_string(),
                model: "gemini-3.1-pro-preview".to_string(),
                provider: "gemini".to_string(),
                clone_id: "cow_4".to_string(),
                clone_path: "/tmp/cow_4".to_string(),
                ok: true,
                summary: "All tests pass quickly".to_string(),
                diffs: vec![sample_diff],
                verify_passed: true,
                verify_results: vec![],
                duration_ms: 1_200,
                score: 0,
            },
        ];

        rank_arena_candidates(&mut candidates);
        let ordered_ids: Vec<&str> = candidates.iter().map(|c| c.candidate_id.as_str()).collect();
        assert_eq!(
            ordered_ids,
            vec![
                "c_fast_pass",
                "c_slow_pass",
                "c_ok_but_verify_fail",
                "c_failed_api"
            ]
        );
        assert!(candidates[0].score > candidates[1].score);
        assert!(candidates[1].score > candidates[2].score);
        assert!(candidates[2].score > candidates[3].score);
    }

    #[test]
    fn goal_decomposer_parses_fenced_dag_and_falls_back_cleanly() {
        let valid_raw = r#"```json
[
  {
    "id": "contracts",
    "title": "Research Contracts",
    "role": "researcher",
    "prompt": "Publish schema",
    "dependsOn": [],
    "writeEnabled": false
  },
  {
    "id": "media_assets",
    "title": "Generate Assets",
    "role": "mediaProducer",
    "prompt": "Create SVG icons",
    "dependsOn": ["contracts"],
    "writeEnabled": true
  },
  {
    "id": "qa_check",
    "title": "Verify Integration",
    "role": "verifier",
    "prompt": "Check outputs",
    "dependsOn": ["media_assets"],
    "writeEnabled": false
  }
]
```"#;
        let parsed = parse_decomposed_dag_json(valid_raw).expect("valid 3-node DAG");
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[1].role, SwarmRole::MediaProducer);

        // Malformed JSON falls back to deterministic 3-node `research -> implement -> verify`.
        assert!(parse_decomposed_dag_json("not valid json").is_err());
        let fallback = fallback_goal_dag("Add OAuth PKCE flow");
        assert_eq!(fallback.len(), 3);
        assert_eq!(fallback[0].id, "research");
        assert_eq!(fallback[1].id, "implement");
        assert_eq!(fallback[2].id, "verify");
        assert_eq!(compute_execution_waves(&fallback).unwrap().len(), 3);
    }

    #[test]
    fn worker_file_tools_enforce_syntax_gate_and_path_confinement() {
        let ws = make_temp_dir("rursor_swarm_syntax_gate");
        fs::create_dir_all(ws.join("src")).unwrap();
        fs::write(ws.join("src/math.rs"), "pub fn double(x: i32) -> i32 {\n    x * 2\n}\n")
            .unwrap();

        // Broken Rust edit is rejected by Tree-sitter syntax gate without modifying disk.
        let bad = edit_confined_file_gated(
            &ws,
            "src/math.rs",
            "x * 2",
            "let broken = ;\n    x * 2",
            false,
        );
        assert!(bad.is_err());
        assert!(fs::read_to_string(ws.join("src/math.rs"))
            .unwrap()
            .contains("x * 2"));

        // Valid Rust edit passes syntax gate and updates disk.
        let good = edit_confined_file_gated(&ws, "src/math.rs", "x * 2", "x.saturating_mul(2)", false);
        assert!(good.is_ok());
        assert!(fs::read_to_string(ws.join("src/math.rs"))
            .unwrap()
            .contains("saturating_mul"));

        // Path traversal outside `work_root` is rejected.
        let escape = write_confined_file_gated(&ws, "../escape.rs", "pub fn nope() {}\n");
        assert!(escape.is_err());

        let _ = fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn worker_tool_dispatch_executes_commands_blackboard_mail_and_staging_merge() {
        let base = make_temp_dir("rursor_swarm_worker_tools");
        let ws = base.join("workspace");
        let clones_dir = base.join("clones");
        fs::create_dir_all(ws.join("src")).unwrap();
        fs::write(ws.join("src/lib.rs"), "pub fn greet() -> &'static str {\n    \"hi\"\n}\n")
            .unwrap();

        let clone_mgr = ApfsCloneManager::with_base_dir(&clones_dir);
        let c1 = clone_mgr.create_clone(&ws, "swarm_coder").unwrap();
        let bb = SwarmBlackboard::new();
        let noop_ev: Arc<dyn Fn(SwarmEvent) + Send + Sync> = Arc::new(|_| {});

        let ro_task = sample_spec("researcher", SwarmRole::Researcher, &[], false);
        let rw_task = sample_spec("coder", SwarmRole::Coder, &["researcher"], true);

        // Read-only worker cannot write files.
        let blocked_write: WorkerToolAction = serde_json::from_value(json!({
            "action": "write_file",
            "path": "src/blocked.rs",
            "content": "pub fn blocked() {}\n"
        }))
        .unwrap();
        let (ok_ro, _, _) = execute_worker_tool_action(
            &blocked_write,
            &ro_task,
            &ws,
            false,
            &bb,
            "m1",
            &noop_ev,
        )
        .await;
        assert!(!ok_ro);

        // Write-enabled worker writes a syntax-gated file, runs a sandboxed command, publishes
        // blackboard state, and sends mail inside its isolated CoW clone.
        let write_act: WorkerToolAction = serde_json::from_value(json!({
            "action": "write_file",
            "path": "src/feature.rs",
            "content": "pub const PORT: u16 = 9090;\n"
        }))
        .unwrap();
        let (ok_w, _, _) = execute_worker_tool_action(
            &write_act,
            &rw_task,
            &c1.clone_root,
            false,
            &bb,
            "m1",
            &noop_ev,
        )
        .await;
        assert!(ok_w);

        let cmd_act: WorkerToolAction = serde_json::from_value(json!({
            "action": "run_command",
            "command": "ls src"
        }))
        .unwrap();
        let (ok_cmd, _, cmd_out) = execute_worker_tool_action(
            &cmd_act,
            &rw_task,
            &c1.clone_root,
            false,
            &bb,
            "m1",
            &noop_ev,
        )
        .await;
        assert!(ok_cmd, "unexpected command output: {cmd_out}");
        assert!(cmd_out.contains("feature.rs"));

        let bb_act: WorkerToolAction = serde_json::from_value(json!({
            "action": "publish_blackboard",
            "key": "service_port",
            "value": "9090"
        }))
        .unwrap();
        let (ok_bb, _, _) = execute_worker_tool_action(
            &bb_act,
            &rw_task,
            &c1.clone_root,
            false,
            &bb,
            "m1",
            &noop_ev,
        )
        .await;
        assert!(ok_bb);
        assert_eq!(bb.get("service_port").unwrap().value, "9090");

        let mail_act: WorkerToolAction = serde_json::from_value(json!({
            "action": "send_mail",
            "to": "verifier",
            "message": "Ready for integration checks"
        }))
        .unwrap();
        let (ok_mail, _, _) = execute_worker_tool_action(
            &mail_act,
            &rw_task,
            &c1.clone_root,
            false,
            &bb,
            "m1",
            &noop_ev,
        )
        .await;
        assert!(ok_mail);
        assert_eq!(bb.drain_mail_for("verifier").len(), 1);

        // Verify staging clone merge produces a valid `staging_clone_id` without mutating `ws`.
        let nodes = vec![SwarmNodeState {
            id: "coder".to_string(),
            title: "Coder".to_string(),
            role: SwarmRole::Coder,
            depends_on: vec![],
            status: SwarmNodeStatus::Completed,
            clone_id: Some(c1.clone_id.clone()),
            clone_path: Some(c1.clone_root.display().to_string()),
            summary: "Added feature.rs".to_string(),
            modified_files: clone_mgr.diff_clone(&c1.clone_id).unwrap(),
            verify_results: vec![],
            duration_ms: 80,
            error: None,
        }];
        let staging_report = merge_worker_clones_to_staging(&nodes, &ws, &clone_mgr).unwrap();
        assert_eq!(staging_report.merged_files, vec!["src/feature.rs".to_string()]);
        assert!(staging_report.staging_clone_id.is_some());
        assert!(!ws.join("src/feature.rs").exists());

        let _ = fs::remove_dir_all(&base);
    }
}
