use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Deserialize)]
pub struct RpcRequest {
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Serialize)]
pub struct RpcResponse {
    pub id: u64,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RpcEvent {
    pub event: String,
    #[serde(rename = "requestId")]
    pub request_id: u64,
    pub payload: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetConfigParams {
    pub api_key: Option<String>,
    pub anthropic_api_key: Option<String>,
    pub openai_api_key: Option<String>,
    pub model: Option<String>,
    pub autocomplete_model: Option<String>,
    pub autocomplete_backend: Option<String>,
    pub local_endpoint: Option<String>,
    pub local_model: Option<String>,
    pub workspace_root: Option<String>,
    pub thinking_budget: Option<u32>,
    pub enable_prompt_cache: Option<bool>,
    pub auto_verify: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexWorkspaceParams {
    pub workspace_root: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateFileParams {
    pub path: String,
    pub content: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoveFileParams {
    pub path: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchContextParams {
    pub query: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrepWorkspaceParams {
    pub query: String,
    #[serde(default)]
    pub case_sensitive: bool,
    #[serde(default = "default_grep_limit")]
    pub limit: usize,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticItem {
    pub path: String,
    pub line: u32,
    pub severity: String,
    pub message: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InlineEditParams {
    pub file_path: String,
    pub language_id: String,
    pub prefix: String,
    pub selection: String,
    pub suffix: String,
    pub instruction: String,
    #[serde(default)]
    pub diagnostics: Vec<DiagnosticItem>,
    pub model: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InlineCompleteParams {
    pub file_path: String,
    pub language_id: String,
    pub prefix: String,
    pub suffix: String,
    #[serde(default)]
    pub recent_files: Vec<ContextSnippet>,
    pub recent_edit_hint: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ImageAttachment {
    pub mime_type: String,
    pub data: String,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    #[serde(default)]
    pub images: Vec<ImageAttachment>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ContextSnippet {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentChatParams {
    pub session_id: Option<String>,
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub context_files: Vec<ContextSnippet>,
    #[serde(default)]
    pub diagnostics: Vec<DiagnosticItem>,
    #[serde(default)]
    pub use_workspace_search: bool,
    #[serde(default)]
    pub include_git_diff: bool,
    #[serde(default = "default_mode")]
    pub mode: String,
    pub model: Option<String>,
    #[serde(default)]
    pub auto_verify: Option<bool>,
    #[serde(default)]
    pub thinking_budget: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NextEditPredictParams {
    pub file_path: String,
    pub language_id: String,
    #[serde(default)]
    pub window_before: String,
    #[serde(default)]
    pub focus_block: String,
    #[serde(default)]
    pub window_after: String,
    #[serde(default = "default_one")]
    pub focus_start_line: usize,
    #[serde(default)]
    pub recent_edits: Vec<String>,
    pub model: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FindCallersParams {
    pub symbol: String,
    #[serde(default = "default_grep_limit")]
    pub limit: usize,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileOutlineParams {
    pub path: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveMemoryParams {
    #[serde(default = "default_workspace_scope")]
    pub scope: String,
    #[serde(default = "default_memory_category")]
    pub category: String,
    pub content: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteMemoryParams {
    pub id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateCheckpointParams {
    pub label: String,
    #[serde(default)]
    pub paths: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreCheckpointParams {
    pub checkpoint_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateWorktreeParams {
    pub task_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreePathParams {
    pub worktree_path: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeWorktreeParams {
    pub branch_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunPtyCommandParams {
    pub command: String,
    pub cwd: Option<String>,
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartBgProcessParams {
    pub command: String,
    pub cwd: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BgProcessIdParams {
    pub proc_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteBgProcessParams {
    pub proc_id: String,
    pub input: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunParallelSubagentsParams {
    pub tasks: Value,
    pub model: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmitToolResultParams {
    pub call_id: String,
    pub ok: bool,
    pub output: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionIdParams {
    pub session_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveSessionParams {
    pub session_id: String,
    pub title: String,
    pub messages: Vec<ChatMessage>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelParams {
    pub request_id: u64,
}

fn default_one() -> usize {
    1
}

fn default_workspace_scope() -> String {
    "workspace".to_string()
}

fn default_memory_category() -> String {
    "general".to_string()
}

fn default_limit() -> usize {
    6
}

fn default_grep_limit() -> usize {
    30
}

fn default_mode() -> String {
    "agent".to_string()
}

impl RpcResponse {
    pub fn ok(id: u64, result: Value) -> Self {
        Self {
            id,
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: u64, error: impl Into<String>) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(error.into()),
        }
    }
}
