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
    pub model: Option<String>,
    pub workspace_root: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexWorkspaceParams {
    pub workspace_root: String,
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
pub struct InlineEditParams {
    pub file_path: String,
    pub language_id: String,
    pub prefix: String,
    pub selection: String,
    pub suffix: String,
    pub instruction: String,
    pub model: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextSnippet {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentChatParams {
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub context_files: Vec<ContextSnippet>,
    #[serde(default)]
    pub use_workspace_search: bool,
    pub model: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelParams {
    pub request_id: u64,
}

fn default_limit() -> usize {
    6
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
