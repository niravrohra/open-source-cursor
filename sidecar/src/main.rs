mod context;
mod gemini;
mod protocol;

use crate::context::WorkspaceIndex;
use crate::gemini::GeminiClient;
use crate::protocol::{
    AgentChatParams, CancelParams, ChatMessage, GrepWorkspaceParams, IndexWorkspaceParams,
    InlineCompleteParams, InlineEditParams, RemoveFileParams, RpcEvent, RpcRequest, RpcResponse,
    SearchContextParams, SetConfigParams, SubmitToolResultParams, UpdateFileParams,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, Mutex, RwLock};
use tokio::task::JoinHandle;

const DEFAULT_GEMINI_MODEL: &str = "gemini-2.5-flash";
const DEFAULT_AUTOCOMPLETE_MODEL: &str = "gemini-2.5-flash-lite";
const MAX_AGENT_STEPS: usize = 12;

static NEXT_TOOL_CALL_ID: AtomicU64 = AtomicU64::new(1);

struct ToolResultPayload {
    ok: bool,
    output: Value,
}

struct AppState {
    api_key: RwLock<Option<String>>,
    model: RwLock<String>,
    autocomplete_model: RwLock<String>,
    index: RwLock<WorkspaceIndex>,
    gemini: GeminiClient,
    in_flight: Mutex<HashMap<u64, JoinHandle<()>>>,
    pending_tools: Mutex<HashMap<String, oneshot::Sender<ToolResultPayload>>>,
}

impl AppState {
    fn new() -> Self {
        let env_key = std::env::var("GEMINI_API_KEY")
            .ok()
            .filter(|s| !s.trim().is_empty());
        Self {
            api_key: RwLock::new(env_key),
            model: RwLock::new(DEFAULT_GEMINI_MODEL.to_string()),
            autocomplete_model: RwLock::new(DEFAULT_AUTOCOMPLETE_MODEL.to_string()),
            index: RwLock::new(WorkspaceIndex::default()),
            gemini: GeminiClient::new(),
            in_flight: Mutex::new(HashMap::new()),
            pending_tools: Mutex::new(HashMap::new()),
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

        if req.method == "submit_tool_result" {
            handle_submit_tool_result(req, state_clone, tx_clone).await;
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

async fn handle_submit_tool_result(
    req: RpcRequest,
    state: Arc<AppState>,
    out_tx: mpsc::UnboundedSender<String>,
) {
    let resp = match serde_json::from_value::<SubmitToolResultParams>(req.params) {
        Ok(params) => {
            let sender = state.pending_tools.lock().await.remove(&params.call_id);
            let delivered = if let Some(tx) = sender {
                tx.send(ToolResultPayload {
                    ok: params.ok,
                    output: params.output,
                })
                .is_ok()
            } else {
                false
            };
            RpcResponse::ok(req.id, json!({ "delivered": delivered }))
        }
        Err(e) => RpcResponse::err(req.id, format!("Invalid submit_tool_result params: {e}")),
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
            let autocomplete_model = state.autocomplete_model.read().await.clone();
            let index = state.index.read().await;
            RpcResponse::ok(
                req.id,
                json!({
                    "ready": true,
                    "hasApiKey": has_key,
                    "model": model,
                    "autocompleteModel": autocomplete_model,
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
            if let Some(ac_model) = params.autocomplete_model {
                if !ac_model.trim().is_empty() {
                    *state.autocomplete_model.write().await = ac_model.trim().to_string();
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
        "update_file" => {
            let params: UpdateFileParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => return RpcResponse::err(req.id, format!("Invalid update_file params: {e}")),
            };
            let mut index = state.index.write().await;
            match index.upsert_file(&PathBuf::from(&params.path), params.content) {
                Ok(updated) => RpcResponse::ok(req.id, json!({ "updated": updated })),
                Err(e) => RpcResponse::err(req.id, e),
            }
        }
        "remove_file" => {
            let params: RemoveFileParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => return RpcResponse::err(req.id, format!("Invalid remove_file params: {e}")),
            };
            let mut index = state.index.write().await;
            let removed = index.remove_file(&PathBuf::from(&params.path));
            RpcResponse::ok(req.id, json!({ "removed": removed }))
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
        "grep_workspace" => {
            let params: GrepWorkspaceParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => {
                    return RpcResponse::err(req.id, format!("Invalid grep_workspace params: {e}"))
                }
            };
            let index = state.index.read().await;
            let hits = index.grep(&params.query, params.case_sensitive, params.limit);
            RpcResponse::ok(req.id, json!({ "hits": hits }))
        }
        "list_files" => {
            let prefix = req
                .params
                .get("prefix")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let limit = req
                .params
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(500) as usize;
            let index = state.index.read().await;
            let files = index.list_files(prefix.as_deref(), limit);
            RpcResponse::ok(req.id, json!({ "files": files }))
        }
        "inline_edit" => handle_inline_edit(req, state, out_tx).await,
        "inline_complete" => handle_inline_complete(req, state).await,
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
    let model = match params.model {
        Some(m) if !m.trim().is_empty() => m,
        _ => state.model.read().await.clone(),
    };

    let mut diag_block = String::new();
    if !params.diagnostics.is_empty() {
        diag_block.push_str("\n\n<diagnostics>\n");
        for d in &params.diagnostics {
            diag_block.push_str(&format!("[{}] line {}: {}\n", d.severity, d.line, d.message));
        }
        diag_block.push_str("</diagnostics>");
    }

    let system_prompt = "You are Rursor's inline code editor. Output ONLY the exact replacement code for the <selection> region—no markdown code fences, no explanations, no surrounding context, and preserve indentation.";
    let user_prompt = format!(
        "File: {file} ({lang})\n\n<prefix>\n{prefix}\n</prefix>\n\n<selection>\n{selection}\n</selection>\n\n<suffix>\n{suffix}\n</suffix>{diag}\n\nInstruction: {instruction}",
        file = params.file_path,
        lang = params.language_id,
        prefix = params.prefix,
        selection = params.selection,
        suffix = params.suffix,
        diag = diag_block,
        instruction = params.instruction
    );

    let messages = vec![ChatMessage {
        role: "user".to_string(),
        content: user_prompt,
    }];

    let req_id = req.id;
    let res = state
        .gemini
        .stream_chat(&api_key, &model, Some(system_prompt), &messages, 0.2, None, |delta| {
            emit_event(out_tx, req_id, "stream_chunk", json!({ "text": delta }));
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

async fn handle_inline_complete(req: RpcRequest, state: Arc<AppState>) -> RpcResponse {
    let params: InlineCompleteParams = match serde_json::from_value(req.params) {
        Ok(p) => p,
        Err(e) => return RpcResponse::err(req.id, format!("Invalid inline_complete params: {e}")),
    };

    let Some(api_key) = state.api_key.read().await.clone() else {
        return RpcResponse::ok(req.id, json!({ "completion": "" }));
    };

    let model = match params.model {
        Some(m) if !m.trim().is_empty() => m,
        _ => state.autocomplete_model.read().await.clone(),
    };

    let mut recent_block = String::new();
    for rf in params.recent_files.iter().take(2) {
        recent_block.push_str(&format!("\n--- Related file: {} ---\n{}\n", rf.path, rf.content));
    }

    let system_prompt = "You are Rursor's low-latency Tab autocomplete engine. Complete the code exactly at <CURSOR>. Output ONLY the raw text to insert at <CURSOR>—no markdown fences, no commentary, do not repeat the prefix or suffix. Return empty output if no confident continuation is needed.";
    let user_prompt = format!(
        "File: {file} ({lang}){recent}\n\n<code_context>\n{prefix}<CURSOR>{suffix}\n</code_context>",
        file = params.file_path,
        lang = params.language_id,
        recent = recent_block,
        prefix = params.prefix,
        suffix = params.suffix
    );

    let messages = vec![ChatMessage {
        role: "user".to_string(),
        content: user_prompt,
    }];

    let res = state
        .gemini
        .stream_chat(&api_key, &model, Some(system_prompt), &messages, 0.1, Some(180), |_| {})
        .await;

    match res {
        Ok(raw) => {
            let completion = clean_fim_completion(&raw, &params.prefix, &params.suffix);
            RpcResponse::ok(req.id, json!({ "completion": completion }))
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
    let model = match params.model {
        Some(m) if !m.trim().is_empty() => m,
        _ => state.model.read().await.clone(),
    };

    let mut context_blocks = String::new();
    for snippet in &params.context_files {
        context_blocks.push_str(&format!("\n--- File: {} ---\n{}\n", snippet.path, snippet.content));
    }

    if !params.diagnostics.is_empty() {
        context_blocks.push_str("\n--- Active Diagnostics ---\n");
        for d in params.diagnostics.iter().take(20) {
            context_blocks.push_str(&format!("{}:{} [{}] {}\n", d.path, d.line, d.severity, d.message));
        }
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

    let req_id = req.id;

    if params.mode == "ask" {
        let system_prompt = "You are Rursor, a concise, accurate AI coding assistant inside a macOS code editor. Reference exact file paths and symbols from context when available, and provide clean, directly applicable code blocks.";
        let res = state
            .gemini
            .stream_chat(&api_key, &model, Some(system_prompt), &messages, 0.3, None, |delta| {
                emit_event(out_tx, req_id, "stream_chunk", json!({ "text": delta }));
            })
            .await;

        return match res {
            Ok(full) => RpcResponse::ok(req.id, json!({ "text": full })),
            Err(e) => RpcResponse::err(req.id, e),
        };
    }

    // Agent mode: multi-step Gemini function calling loop
    let mut contents: Vec<Value> = messages
        .iter()
        .map(|m| {
            let role = match m.role.as_str() {
                "assistant" | "model" => "model",
                _ => "user",
            };
            json!({
                "role": role,
                "parts": [{ "text": m.content }]
            })
        })
        .collect();

    let tools = Some(agent_tool_declarations());
    let workspace_root_label = state
        .index
        .read()
        .await
        .root
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "none".to_string());

    let system_prompt = format!(
        "You are Rursor Agent, an autonomous AI software engineer inside a macOS code editor (workspace root: {workspace_root_label}).\n\
        Use the provided tools to inspect files, search/grep the workspace, edit or create files, and run terminal commands to verify changes.\n\
        Rules:\n\
        - Always read a file before editing it unless creating a brand new file.\n\
        - Prefer minimal, exact replacements in edit_file (`old_text` must uniquely match existing text in the file).\n\
        - Summarize the exact files modified and any verification commands run."
    );

    let mut combined_text = String::new();

    for step in 0..MAX_AGENT_STEPS {
        if step > 0 && !combined_text.is_empty() && !combined_text.ends_with('\n') {
            emit_event(out_tx, req_id, "stream_chunk", json!({ "text": "\n" }));
            combined_text.push('\n');
        }

        let turn = match state
            .gemini
            .stream_turn(
                &api_key,
                &model,
                Some(&system_prompt),
                &contents,
                tools.clone(),
                0.2,
                None,
                |delta| {
                    emit_event(out_tx, req_id, "stream_chunk", json!({ "text": delta }));
                },
            )
            .await
        {
            Ok(t) => t,
            Err(e) => return RpcResponse::err(req.id, e),
        };

        combined_text.push_str(&turn.text);

        if turn.function_calls.is_empty() {
            return RpcResponse::ok(req.id, json!({ "text": combined_text }));
        }

        contents.push(json!({
            "role": "model",
            "parts": coalesce_model_parts(turn.raw_model_parts)
        }));

        let mut response_parts = Vec::new();
        for fc in turn.function_calls {
            let call_id = format!("tc_{}", NEXT_TOOL_CALL_ID.fetch_add(1, Ordering::Relaxed));
            let result_val = execute_agent_tool(
                req_id,
                &call_id,
                &fc.name,
                &fc.args,
                &state,
                out_tx,
            )
            .await;

            response_parts.push(json!({
                "functionResponse": {
                    "name": fc.name,
                    "response": result_val
                }
            }));
        }

        contents.push(json!({
            "role": "user",
            "parts": response_parts
        }));
    }

    let suffix = "\n\n[Stopped after reaching max agent steps (12).]";
    emit_event(out_tx, req_id, "stream_chunk", json!({ "text": suffix }));
    combined_text.push_str(suffix);
    RpcResponse::ok(req.id, json!({ "text": combined_text }))
}

async fn execute_agent_tool(
    req_id: u64,
    call_id: &str,
    name: &str,
    args: &Value,
    state: &Arc<AppState>,
    out_tx: &mpsc::UnboundedSender<String>,
) -> Value {
    match name {
        "read_file" | "list_files" | "search_workspace" | "grep_workspace" => {
            emit_event(
                out_tx,
                req_id,
                "tool_event",
                json!({
                    "callId": call_id,
                    "phase": "start",
                    "name": name,
                    "args": args,
                }),
            );

            let (ok, output) = {
                let index = state.index.read().await;
                match name {
                    "read_file" => {
                        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
                        let start = args.get("start_line").and_then(|v| v.as_u64()).map(|n| n as usize);
                        let end = args.get("end_line").and_then(|v| v.as_u64()).map(|n| n as usize);
                        match index.read_file(path, start, end) {
                            Ok(text) => (true, json!({ "content": text })),
                            Err(e) => (false, json!({ "error": e })),
                        }
                    }
                    "list_files" => {
                        let prefix = args.get("prefix").and_then(|v| v.as_str());
                        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(200) as usize;
                        let files = index.list_files(prefix, limit);
                        (true, json!({ "files": files, "count": files.len() }))
                    }
                    "search_workspace" => {
                        let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
                        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(8) as usize;
                        let hits = index.search(query, limit);
                        (true, json!({ "hits": hits }))
                    }
                    "grep_workspace" => {
                        let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
                        let case_sensitive = args
                            .get("case_sensitive")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(30) as usize;
                        let hits = index.grep(query, case_sensitive, limit);
                        (true, json!({ "hits": hits }))
                    }
                    _ => unreachable!(),
                }
            };

            emit_event(
                out_tx,
                req_id,
                "tool_event",
                json!({
                    "callId": call_id,
                    "phase": "done",
                    "name": name,
                    "ok": ok,
                    "output": output,
                }),
            );

            json!({ "ok": ok, "result": output })
        }
        "edit_file" | "write_file" | "run_command" => {
            let (tx, rx) = oneshot::channel::<ToolResultPayload>();
            state.pending_tools.lock().await.insert(call_id.to_string(), tx);

            emit_event(
                out_tx,
                req_id,
                "tool_request",
                json!({
                    "callId": call_id,
                    "name": name,
                    "args": args,
                }),
            );

            match rx.await {
                Ok(res) => json!({ "ok": res.ok, "result": res.output }),
                Err(_) => json!({ "ok": false, "result": { "error": "Tool execution cancelled." } }),
            }
        }
        unknown => json!({
            "ok": false,
            "result": { "error": format!("Unknown tool: {unknown}") }
        }),
    }
}

fn agent_tool_declarations() -> Vec<Value> {
    vec![json!({
        "functionDeclarations": [
            {
                "name": "read_file",
                "description": "Read a file inside the workspace with 1-based line numbers.",
                "parameters": {
                    "type": "OBJECT",
                    "properties": {
                        "path": { "type": "STRING", "description": "Workspace-relative path to the file." },
                        "start_line": { "type": "INTEGER", "description": "Optional 1-based starting line." },
                        "end_line": { "type": "INTEGER", "description": "Optional 1-based inclusive ending line." }
                    },
                    "required": ["path"]
                }
            },
            {
                "name": "list_files",
                "description": "List indexed files in the workspace, optionally filtered by directory prefix.",
                "parameters": {
                    "type": "OBJECT",
                    "properties": {
                        "prefix": { "type": "STRING", "description": "Optional relative directory prefix." },
                        "limit": { "type": "INTEGER", "description": "Max files to return (default 200)." }
                    }
                }
            },
            {
                "name": "search_workspace",
                "description": "Rank workspace files matching keywords in path or contents.",
                "parameters": {
                    "type": "OBJECT",
                    "properties": {
                        "query": { "type": "STRING", "description": "Space-separated keywords or symbol names." },
                        "limit": { "type": "INTEGER", "description": "Max hits to return (default 8)." }
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "grep_workspace",
                "description": "Find exact line matches across indexed workspace files.",
                "parameters": {
                    "type": "OBJECT",
                    "properties": {
                        "query": { "type": "STRING", "description": "Substring to search for." },
                        "case_sensitive": { "type": "BOOLEAN", "description": "Case sensitivity (default false)." },
                        "limit": { "type": "INTEGER", "description": "Max line matches to return (default 30)." }
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "edit_file",
                "description": "Propose an exact replacement in an existing workspace file. Requires old_text to match uniquely.",
                "parameters": {
                    "type": "OBJECT",
                    "properties": {
                        "path": { "type": "STRING", "description": "Workspace-relative file path." },
                        "old_text": { "type": "STRING", "description": "Exact existing snippet to replace." },
                        "new_text": { "type": "STRING", "description": "Replacement code." },
                        "explanation": { "type": "STRING", "description": "Short summary of why this edit is needed." }
                    },
                    "required": ["path", "old_text", "new_text"]
                }
            },
            {
                "name": "write_file",
                "description": "Create a new file or overwrite an entire file in the workspace.",
                "parameters": {
                    "type": "OBJECT",
                    "properties": {
                        "path": { "type": "STRING", "description": "Workspace-relative file path." },
                        "content": { "type": "STRING", "description": "Complete file contents." },
                        "explanation": { "type": "STRING", "description": "Short summary of why this file is written." }
                    },
                    "required": ["path", "content"]
                }
            },
            {
                "name": "run_command",
                "description": "Ask the user to run a shell command in the workspace root (e.g., cargo test, npm run compile) and return stdout/stderr/exit code.",
                "parameters": {
                    "type": "OBJECT",
                    "properties": {
                        "command": { "type": "STRING", "description": "Shell command to execute in workspace root." },
                        "reason": { "type": "STRING", "description": "Why this command should be run." }
                    },
                    "required": ["command"]
                }
            }
        ]
    })]
}

fn coalesce_model_parts(parts: Vec<Value>) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for part in parts {
        let is_plain_text = part.as_object().is_some_and(|obj| {
            obj.len() == 1 && obj.get("text").and_then(|v| v.as_str()).is_some()
        });
        if is_plain_text {
            let chunk = part["text"].as_str().unwrap_or("");
            if let Some(last) = out.last_mut() {
                if let Some(obj) = last.as_object_mut() {
                    if obj.len() == 1 {
                        if let Some(prev) = obj.get("text").and_then(|v| v.as_str()) {
                            let merged = format!("{prev}{chunk}");
                            obj.insert("text".to_string(), Value::String(merged));
                            continue;
                        }
                    }
                }
            }
        }
        out.push(part);
    }
    out
}

fn emit_event(out_tx: &mpsc::UnboundedSender<String>, request_id: u64, event: &str, payload: Value) {
    let ev = RpcEvent {
        event: event.to_string(),
        request_id,
        payload,
    };
    let _ = out_tx.send(serde_json::to_string(&ev).unwrap_or_default());
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

fn clean_fim_completion(raw: &str, prefix: &str, suffix: &str) -> String {
    let mut text = strip_code_fences(raw);
    if text.trim().is_empty() {
        return String::new();
    }

    // Remove accidental overlap with the tail of prefix on the current line.
    let current_line_prefix = prefix.rsplit('\n').next().unwrap_or("");
    if !current_line_prefix.is_empty() && text.starts_with(current_line_prefix) {
        text = text[current_line_prefix.len()..].to_string();
    }

    // Remove accidental overlap with the start of suffix.
    let first_suffix_line = suffix.split('\n').next().unwrap_or("").trim_end();
    if !first_suffix_line.is_empty() && text.ends_with(first_suffix_line) {
        let new_len = text.len() - first_suffix_line.len();
        text.truncate(new_len);
    }

    text
}

#[cfg(test)]
mod tests {
    use super::{clean_fim_completion, coalesce_model_parts, strip_code_fences};
    use serde_json::json;

    #[test]
    fn strips_markdown_fences_from_inline_edit() {
        let raw = "```rust\nlet x = 42;\n```";
        assert_eq!(strip_code_fences(raw), "let x = 42;");
    }

    #[test]
    fn cleans_fim_overlap() {
        let out = clean_fim_completion("```ts\nconst x = 42;\n```", "const x = ", ";");
        assert_eq!(out, "42");
    }

    #[test]
    fn coalesces_plain_text_parts_and_keeps_function_calls() {
        let parts = vec![
            json!({"text": "Hello "}),
            json!({"text": "world"}),
            json!({"functionCall": {"name": "read_file", "args": {"path": "a.rs"}}, "thoughtSignature": "sig"}),
        ];
        let coalesced = coalesce_model_parts(parts);
        assert_eq!(coalesced.len(), 2);
        assert_eq!(coalesced[0]["text"], "Hello world");
        assert_eq!(coalesced[1]["thoughtSignature"], "sig");
    }
}
