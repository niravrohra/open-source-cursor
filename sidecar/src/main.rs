mod context;
mod gemini;
mod protocol;

use crate::context::WorkspaceIndex;
use crate::gemini::GeminiClient;
use crate::protocol::{
    AgentChatParams, CancelParams, ChatMessage, IndexWorkspaceParams, InlineEditParams, RpcEvent,
    RpcRequest, RpcResponse, SearchContextParams, SetConfigParams,
};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio::task::JoinHandle;

const DEFAULT_GEMINI_MODEL: &str = "gemini-2.5-flash";

struct AppState {
    api_key: RwLock<Option<String>>,
    model: RwLock<String>,
    index: RwLock<WorkspaceIndex>,
    gemini: GeminiClient,
    in_flight: Mutex<HashMap<u64, JoinHandle<()>>>,
}

impl AppState {
    fn new() -> Self {
        let env_key = std::env::var("GEMINI_API_KEY")
            .ok()
            .filter(|s| !s.trim().is_empty());
        Self {
            api_key: RwLock::new(env_key),
            model: RwLock::new(DEFAULT_GEMINI_MODEL.to_string()),
            index: RwLock::new(WorkspaceIndex::default()),
            gemini: GeminiClient::new(),
            in_flight: Mutex::new(HashMap::new()),
        }
    }
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let state = Arc::new(AppState::new());
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();

    let writer_handle = tokio::spawn(async move {
        let mut stdout = io::stdout();
        while let Some(line) = out_rx.recv().await {
            if stdout.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            if stdout.write_all(b"\n").await.is_err() {
                break;
            }
            let _ = stdout.flush().await;
        }
    });

    let stdin = BufReader::new(io::stdin());
    let mut lines = stdin.lines();

    while let Some(line) = lines.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let req: RpcRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                let err_resp = RpcResponse::err(0, format!("Invalid JSON-RPC request: {e}"));
                let _ = out_tx.send(serde_json::to_string(&err_resp).unwrap_or_default());
                continue;
            }
        };

        let req_id = req.id;
        let state_clone = Arc::clone(&state);
        let tx_clone = out_tx.clone();

        if req.method == "cancel" {
            handle_cancel(req, state_clone, tx_clone).await;
            continue;
        }

        let handle = tokio::spawn(async move {
            let resp = dispatch(req, Arc::clone(&state_clone), &tx_clone).await;
            let _ = tx_clone.send(serde_json::to_string(&resp).unwrap_or_default());
            state_clone.in_flight.lock().await.remove(&req_id);
        });

        state.in_flight.lock().await.insert(req_id, handle);
    }

    drop(out_tx);
    let _ = writer_handle.await;
    Ok(())
}

async fn handle_cancel(
    req: RpcRequest,
    state: Arc<AppState>,
    out_tx: mpsc::UnboundedSender<String>,
) {
    let resp = match serde_json::from_value::<CancelParams>(req.params) {
        Ok(params) => {
            let cancelled = if let Some(handle) = state.in_flight.lock().await.remove(&params.request_id) {
                handle.abort();
                true
            } else {
                false
            };
            RpcResponse::ok(req.id, json!({ "cancelled": cancelled }))
        }
        Err(e) => RpcResponse::err(req.id, format!("Invalid cancel params: {e}")),
    };
    let _ = out_tx.send(serde_json::to_string(&resp).unwrap_or_default());
}

async fn dispatch(
    req: RpcRequest,
    state: Arc<AppState>,
    out_tx: &mpsc::UnboundedSender<String>,
) -> RpcResponse {
    match req.method.as_str() {
        "ping" => RpcResponse::ok(
            req.id,
            json!({
                "service": "rursor-sidecar",
                "version": env!("CARGO_PKG_VERSION"),
            }),
        ),
        "status" => {
            let has_key = state.api_key.read().await.is_some();
            let model = state.model.read().await.clone();
            let index = state.index.read().await;
            RpcResponse::ok(
                req.id,
                json!({
                    "ready": true,
                    "hasApiKey": has_key,
                    "model": model,
                    "indexedFiles": index.files.len(),
                    "workspaceRoot": index.root.as_ref().map(|p| p.display().to_string()),
                }),
            )
        }
        "set_config" => {
            let params: SetConfigParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => return RpcResponse::err(req.id, format!("Invalid set_config params: {e}")),
            };
            if let Some(key) = params.api_key {
                let cleaned = key.trim().to_string();
                *state.api_key.write().await = if cleaned.is_empty() {
                    None
                } else {
                    Some(cleaned)
                };
            }
            if let Some(model) = params.model {
                if !model.trim().is_empty() {
                    *state.model.write().await = model.trim().to_string();
                }
            }
            if let Some(root) = params.workspace_root {
                let mut index = state.index.write().await;
                if let Err(e) = index.index_directory(&PathBuf::from(root)) {
                    return RpcResponse::err(req.id, e);
                }
            }
            RpcResponse::ok(req.id, json!({ "updated": true }))
        }
        "index_workspace" => {
            let params: IndexWorkspaceParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => {
                    return RpcResponse::err(req.id, format!("Invalid index_workspace params: {e}"))
                }
            };
            let mut index = state.index.write().await;
            match index.index_directory(&PathBuf::from(&params.workspace_root)) {
                Ok(count) => RpcResponse::ok(req.id, json!({ "indexedFiles": count })),
                Err(e) => RpcResponse::err(req.id, e),
            }
        }
        "search_context" => {
            let params: SearchContextParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => {
                    return RpcResponse::err(req.id, format!("Invalid search_context params: {e}"))
                }
            };
            let index = state.index.read().await;
            let hits = index.search(&params.query, params.limit);
            RpcResponse::ok(req.id, json!({ "hits": hits }))
        }
        "inline_edit" => handle_inline_edit(req, state, out_tx).await,
        "agent_chat" => handle_agent_chat(req, state, out_tx).await,
        other => RpcResponse::err(req.id, format!("Unknown method: {other}")),
    }
}

async fn handle_inline_edit(
    req: RpcRequest,
    state: Arc<AppState>,
    out_tx: &mpsc::UnboundedSender<String>,
) -> RpcResponse {
    let params: InlineEditParams = match serde_json::from_value(req.params) {
        Ok(p) => p,
        Err(e) => return RpcResponse::err(req.id, format!("Invalid inline_edit params: {e}")),
    };

    let Some(api_key) = state.api_key.read().await.clone() else {
        return RpcResponse::err(req.id, "Gemini API key is not configured. Run 'Rursor: Set Gemini API Key'.");
    };
    let model = params
        .model
        .unwrap_or_else(|| state.model.try_read().map(|m| m.clone()).unwrap_or_else(|_| DEFAULT_GEMINI_MODEL.into()));

    let system_prompt = "You are Rursor's inline code editor. Output ONLY the exact replacement code for the selected region—no markdown code fences, no explanations, no surrounding context.";
    let user_prompt = format!(
        "File: {file} ({lang})\n\n<prefix>\n{prefix}\n</prefix>\n\n<selection>\n{selection}\n</selection>\n\n<suffix>\n{suffix}\n</suffix>\n\nInstruction: {instruction}",
        file = params.file_path,
        lang = params.language_id,
        prefix = params.prefix,
        selection = params.selection,
        suffix = params.suffix,
        instruction = params.instruction
    );

    let messages = vec![ChatMessage {
        role: "user".to_string(),
        content: user_prompt,
    }];

    let req_id = req.id;
    let res = state
        .gemini
        .stream_chat(&api_key, &model, Some(system_prompt), &messages, 0.2, |delta| {
            let ev = RpcEvent {
                event: "stream_chunk".to_string(),
                request_id: req_id,
                payload: json!({ "text": delta }),
            };
            let _ = out_tx.send(serde_json::to_string(&ev).unwrap_or_default());
        })
        .await;

    match res {
        Ok(full) => {
            let cleaned = strip_code_fences(&full);
            RpcResponse::ok(req.id, json!({ "replacement": cleaned }))
        }
        Err(e) => RpcResponse::err(req.id, e),
    }
}

async fn handle_agent_chat(
    req: RpcRequest,
    state: Arc<AppState>,
    out_tx: &mpsc::UnboundedSender<String>,
) -> RpcResponse {
    let params: AgentChatParams = match serde_json::from_value(req.params) {
        Ok(p) => p,
        Err(e) => return RpcResponse::err(req.id, format!("Invalid agent_chat params: {e}")),
    };

    let Some(api_key) = state.api_key.read().await.clone() else {
        return RpcResponse::err(req.id, "Gemini API key is not configured. Run 'Rursor: Set Gemini API Key'.");
    };
    let model = params
        .model
        .unwrap_or_else(|| state.model.try_read().map(|m| m.clone()).unwrap_or_else(|_| DEFAULT_GEMINI_MODEL.into()));

    let mut context_blocks = String::new();
    for snippet in &params.context_files {
        context_blocks.push_str(&format!("\n--- File: {} ---\n{}\n", snippet.path, snippet.content));
    }

    if params.use_workspace_search {
        if let Some(last_user) = params.messages.iter().rev().find(|m| m.role == "user") {
            let hits = state.index.read().await.search(&last_user.content, 5);
            for hit in hits {
                context_blocks.push_str(&format!(
                    "\n--- Workspace Match: {} (score {}) ---\n{}\n",
                    hit.path, hit.score, hit.preview
                ));
            }
        }
    }

    let mut messages = params.messages;
    if !context_blocks.is_empty() {
        if let Some(last) = messages.last_mut() {
            last.content = format!("Workspace Context:\n{context_blocks}\n\nRequest:\n{}", last.content);
        }
    }

    let system_prompt = "You are Rursor, a concise, accurate AI coding assistant inside a macOS code editor. Reference exact file paths and symbols from context when available, and provide clean, directly applicable code blocks.";
    let req_id = req.id;

    let res = state
        .gemini
        .stream_chat(&api_key, &model, Some(system_prompt), &messages, 0.3, |delta| {
            let ev = RpcEvent {
                event: "stream_chunk".to_string(),
                request_id: req_id,
                payload: json!({ "text": delta }),
            };
            let _ = out_tx.send(serde_json::to_string(&ev).unwrap_or_default());
        })
        .await;

    match res {
        Ok(full) => RpcResponse::ok(req.id, json!({ "text": full })),
        Err(e) => RpcResponse::err(req.id, e),
    }
}

fn strip_code_fences(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("```") {
        let body = match rest.find('\n') {
            Some(idx) => &rest[idx + 1..],
            None => rest,
        };
        if let Some(without_end) = body.strip_suffix("```") {
            return without_end.trim_end_matches('\n').to_string();
        }
    }
    raw.to_string()
}

#[cfg(test)]
mod tests {
    use super::strip_code_fences;

    #[test]
    fn strips_markdown_fences_from_inline_edit() {
        let raw = "```rust\nlet x = 42;\n```";
        assert_eq!(strip_code_fences(raw), "let x = 42;");
    }
}
