mod browser;
mod context;
mod gemini;
mod local_llm;
mod macos_sandbox;
mod mcp;
mod media_studio;
mod memory;
mod nes;
mod protocol;
mod pty_runner;
mod repomap;
mod research;
mod router;
mod rules;
mod sessions;
mod subagents;
mod swarm;
mod syntax_gate;
mod worktree;

use crate::browser::{
    discover_chromium_binary, discover_ffmpeg_binary, BrowserSessionManager,
};
use crate::context::WorkspaceIndex;
use crate::gemini::{
    chat_message_to_gemini_content, estimate_messages_tokens, GeminiClient, GeminiTurnOptions,
};
use crate::local_llm::LocalLlmClient;
use crate::macos_sandbox::{
    classify_command_risk, run_sandboxed_command_async, ApfsCloneManager, PermissionProfile,
};
use crate::mcp::McpManager;
use crate::media_studio::{
    create_document, create_presentation, create_spreadsheet, dispatch_media_studio_rpc,
    generate_image_asset, render_explainer_video, DocumentSpec, ImageAssetSpec, PresentationSpec,
    SpreadsheetSpec, VideoStoryboardSpec,
};
use crate::memory::{
    delete_memory, format_memory_prompt, list_skills, load_memories, resolve_slash_command,
    save_memory,
};
use crate::nes::{build_nes_prompt, find_related_edit_sites, parse_nes_response};
use crate::protocol::{
    AgentChatParams, BgProcessIdParams, CancelParams, ChatMessage, CowCloneIdParams,
    CreateCheckpointParams, CreateCowCloneParams, CreateWorktreeParams, DeleteMemoryParams,
    FileOutlineParams, FindCallersParams, GrepWorkspaceParams, IndexWorkspaceParams,
    InlineCompleteParams, InlineEditParams, MergeWorktreeParams, NextEditPredictParams,
    RemoveFileParams, RepoMapParams, RestoreCheckpointParams, RpcEvent, RpcRequest, RpcResponse,
    RunParallelSubagentsParams, RunPtyCommandParams, RunSandboxedCommandParams, SaveMemoryParams,
    SaveSessionParams, SearchContextParams, SessionIdParams, SetConfigParams, StartBgProcessParams,
    SubmitToolResultParams, UpdateFileParams, ValidateSyntaxParams, WorktreePathParams,
    WriteBgProcessParams,
};
use crate::pty_runner::{run_pty_command, PtyManager};
use crate::repomap::{build_pagerank_repo_map, RepoFileInput, RepoMapSeed};
use crate::research::{
    builtin_mcp_catalog, fetch_url_as_markdown, install_mcp_preset, web_search, DocsIndexStore,
};
use crate::router::{
    route_task, stream_anthropic_chat, stream_openai_chat, ProviderKeys, ProviderKind, TaskKind,
};
use crate::rules::load_workspace_rules;
use crate::sessions::SessionStore;
use crate::subagents::{
    all_verify_passed, detect_verify_commands, format_verify_feedback, run_parallel_subagents,
    SubagentTask, VerifyResult,
};
use crate::swarm::{
    cleanup_swarm_clones, compute_execution_waves, merge_worker_clones_to_staging,
    merge_worker_clones_to_workspace, run_model_arena_with_events, run_swarm_mission, SmartRouter,
    SwarmNodeState, SwarmTaskSpec,
};
use crate::syntax_gate::{is_syntax_gate_supported, validate_edit_syntax};
use crate::worktree::{
    create_worktree, diff_worktree, list_worktrees, merge_worktree, remove_worktree,
    CheckpointStore,
};
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, Mutex, RwLock};
use tokio::task::JoinHandle;

const DEFAULT_GEMINI_MODEL: &str = "gemini-3.8-flash";
const DEFAULT_AUTOCOMPLETE_MODEL: &str = "gemini-3.5-flash-lite";
const DEFAULT_LOCAL_ENDPOINT: &str = "http://127.0.0.1:11434";
const DEFAULT_LOCAL_MODEL: &str = "qwen2.5-coder:7b";
const MAX_AGENT_STEPS: usize = 16;
const COMPACTION_TOKEN_THRESHOLD: usize = 48_000;
const MIN_PROMPT_CACHE_CHARS: usize = 4_096;

static NEXT_TOOL_CALL_ID: AtomicU64 = AtomicU64::new(1);

struct ToolResultPayload {
    ok: bool,
    output: Value,
}

struct AppState {
    api_key: RwLock<Option<String>>,
    anthropic_key: RwLock<Option<String>>,
    openai_key: RwLock<Option<String>>,
    model: RwLock<String>,
    autocomplete_model: RwLock<String>,
    autocomplete_backend: RwLock<String>,
    local_endpoint: RwLock<String>,
    local_model: RwLock<String>,
    thinking_budget: RwLock<Option<u32>>,
    enable_prompt_cache: RwLock<bool>,
    auto_verify: RwLock<bool>,
    permission_profile: RwLock<PermissionProfile>,
    sandbox_allow_network: RwLock<bool>,
    workspace_rules: RwLock<Option<String>>,
    index: RwLock<WorkspaceIndex>,
    http: Client,
    gemini: GeminiClient,
    local_llm: LocalLlmClient,
    mcp: RwLock<McpManager>,
    sessions: SessionStore,
    checkpoints: CheckpointStore,
    cow_clones: ApfsCloneManager,
    pty_manager: PtyManager,
    browser: BrowserSessionManager,
    docs_index: DocsIndexStore,
    in_flight: Mutex<HashMap<u64, JoinHandle<()>>>,
    pending_tools: Mutex<HashMap<String, oneshot::Sender<ToolResultPayload>>>,
}

impl AppState {
    fn new() -> Self {
        let env_key = std::env::var("GEMINI_API_KEY")
            .ok()
            .filter(|s| !s.trim().is_empty());
        let env_anthropic = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|s| !s.trim().is_empty());
        let env_openai = std::env::var("OPENAI_API_KEY")
            .ok()
            .filter(|s| !s.trim().is_empty());
        Self {
            api_key: RwLock::new(env_key),
            anthropic_key: RwLock::new(env_anthropic),
            openai_key: RwLock::new(env_openai),
            model: RwLock::new(DEFAULT_GEMINI_MODEL.to_string()),
            autocomplete_model: RwLock::new(DEFAULT_AUTOCOMPLETE_MODEL.to_string()),
            autocomplete_backend: RwLock::new("gemini".to_string()),
            local_endpoint: RwLock::new(DEFAULT_LOCAL_ENDPOINT.to_string()),
            local_model: RwLock::new(DEFAULT_LOCAL_MODEL.to_string()),
            thinking_budget: RwLock::new(None),
            enable_prompt_cache: RwLock::new(true),
            auto_verify: RwLock::new(false),
            permission_profile: RwLock::new(PermissionProfile::SafeAuto),
            sandbox_allow_network: RwLock::new(false),
            workspace_rules: RwLock::new(None),
            index: RwLock::new(WorkspaceIndex::default()),
            http: Client::new(),
            gemini: GeminiClient::new(),
            local_llm: LocalLlmClient::new(),
            mcp: RwLock::new(McpManager::default()),
            sessions: SessionStore::default_store(),
            checkpoints: CheckpointStore::default_store(),
            cow_clones: ApfsCloneManager::default_store(),
            pty_manager: PtyManager::new(),
            browser: BrowserSessionManager::default(),
            docs_index: DocsIndexStore::default_store(),
            in_flight: Mutex::new(HashMap::new()),
            pending_tools: Mutex::new(HashMap::new()),
        }
    }

    async fn smart_router(&self) -> SmartRouter {
        SmartRouter::with_local(
            self.http.clone(),
            self.gemini.clone(),
            self.autocomplete_backend.read().await.clone(),
            self.local_endpoint.read().await.clone(),
            self.local_model.read().await.clone(),
        )
    }

    async fn provider_keys(&self) -> ProviderKeys {
        ProviderKeys {
            gemini: self.api_key.read().await.clone(),
            anthropic: self.anthropic_key.read().await.clone(),
            openai: self.openai_key.read().await.clone(),
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
            let has_anthropic_key = state.anthropic_key.read().await.is_some();
            let has_openai_key = state.openai_key.read().await.is_some();
            let model = state.model.read().await.clone();
            let autocomplete_model = state.autocomplete_model.read().await.clone();
            let autocomplete_backend = state.autocomplete_backend.read().await.clone();
            let thinking_budget = *state.thinking_budget.read().await;
            let auto_verify = *state.auto_verify.read().await;
            let permission_profile = *state.permission_profile.read().await;
            let sandbox_allow_network = *state.sandbox_allow_network.read().await;
            let cow_clones_count = state.cow_clones.list_clones().len();
            let has_rules = state.workspace_rules.read().await.is_some();
            let mcp_servers = state.mcp.read().await.statuses();
            let bg_processes = state.pty_manager.list_processes();
            let index = state.index.read().await;
            let skills_count = list_skills(index.root.as_deref()).len();
            let memories_count = load_memories(index.root.as_deref()).len();
            let chromium = discover_chromium_binary()
                .ok()
                .map(|p| p.display().to_string());
            let ffmpeg = discover_ffmpeg_binary()
                .ok()
                .map(|p| p.display().to_string());
            let docs_sources = state.docs_index.list_sources().unwrap_or_default().len();
            RpcResponse::ok(
                req.id,
                json!({
                    "ready": true,
                    "hasApiKey": has_key,
                    "hasAnthropicKey": has_anthropic_key,
                    "hasOpenAiKey": has_openai_key,
                    "model": model,
                    "autocompleteModel": autocomplete_model,
                    "autocompleteBackend": autocomplete_backend,
                    "thinkingBudget": thinking_budget,
                    "autoVerify": auto_verify,
                    "permissionProfile": permission_profile,
                    "sandboxAllowNetwork": sandbox_allow_network,
                    "cowClonesCount": cow_clones_count,
                    "indexedFiles": index.files.len(),
                    "hasWorkspaceRules": has_rules,
                    "mcpServers": mcp_servers,
                    "skillsCount": skills_count,
                    "memoriesCount": memories_count,
                    "backgroundProcesses": bg_processes.len(),
                    "workspaceRoot": index.root.as_ref().map(|p| p.display().to_string()),
                    "omniStudio": {
                        "swarm": true,
                        "browser": {
                            "chromium": chromium,
                            "ffmpeg": ffmpeg,
                        },
                        "media": true,
                        "docsSources": docs_sources,
                        "mcpCatalogPresets": builtin_mcp_catalog().len(),
                        "canvas": true,
                    },
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
            if let Some(key) = params.anthropic_api_key {
                let cleaned = key.trim().to_string();
                *state.anthropic_key.write().await = if cleaned.is_empty() {
                    None
                } else {
                    Some(cleaned)
                };
            }
            if let Some(key) = params.openai_api_key {
                let cleaned = key.trim().to_string();
                *state.openai_key.write().await = if cleaned.is_empty() {
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
            if let Some(backend) = params.autocomplete_backend {
                if !backend.trim().is_empty() {
                    *state.autocomplete_backend.write().await = backend.trim().to_lowercase();
                }
            }
            if let Some(endpoint) = params.local_endpoint {
                if !endpoint.trim().is_empty() {
                    *state.local_endpoint.write().await = endpoint.trim().to_string();
                }
            }
            if let Some(local_model) = params.local_model {
                if !local_model.trim().is_empty() {
                    *state.local_model.write().await = local_model.trim().to_string();
                }
            }
            if let Some(budget) = params.thinking_budget {
                *state.thinking_budget.write().await = if budget == 0 { None } else { Some(budget) };
            }
            if let Some(cache) = params.enable_prompt_cache {
                *state.enable_prompt_cache.write().await = cache;
            }
            if let Some(auto_verify) = params.auto_verify {
                *state.auto_verify.write().await = auto_verify;
            }
            if let Some(profile_str) = params.permission_profile {
                if let Some(parsed) = PermissionProfile::from_str_name(&profile_str) {
                    *state.permission_profile.write().await = parsed;
                }
            }
            if let Some(allow_net) = params.sandbox_allow_network {
                *state.sandbox_allow_network.write().await = allow_net;
            }
            if let Some(root) = params.workspace_root {
                let root_path = PathBuf::from(&root);
                {
                    let mut index = state.index.write().await;
                    if let Err(e) = index.index_directory(&root_path) {
                        return RpcResponse::err(req.id, e);
                    }
                }
                *state.workspace_rules.write().await = load_workspace_rules(&root_path);
                state.browser.set_workspace_root(Some(root_path.clone())).await;
                let _ = state.mcp.write().await.reload(Some(&root_path)).await;
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
            let root_path = PathBuf::from(&params.workspace_root);
            let count = {
                let mut index = state.index.write().await;
                match index.index_directory(&root_path) {
                    Ok(c) => c,
                    Err(e) => return RpcResponse::err(req.id, e),
                }
            };
            *state.workspace_rules.write().await = load_workspace_rules(&root_path);
            state.browser.set_workspace_root(Some(root_path)).await;
            RpcResponse::ok(req.id, json!({ "indexedFiles": count }))
        }
        "reload_mcp" => {
            let root = state.index.read().await.root.clone();
            let statuses = state.mcp.write().await.reload(root.as_deref()).await;
            RpcResponse::ok(req.id, json!({ "servers": statuses }))
        }
        "mcp_status" => {
            let statuses = state.mcp.read().await.statuses();
            RpcResponse::ok(req.id, json!({ "servers": statuses }))
        }
        "list_sessions" => {
            let root = state.index.read().await.root.clone();
            let sessions = state.sessions.list(root.as_deref());
            RpcResponse::ok(req.id, json!({ "sessions": sessions }))
        }
        "load_session" => {
            let params: SessionIdParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => return RpcResponse::err(req.id, format!("Invalid load_session params: {e}")),
            };
            let root = state.index.read().await.root.clone();
            match state.sessions.load(root.as_deref(), &params.session_id) {
                Ok(sess) => RpcResponse::ok(req.id, json!({ "session": sess })),
                Err(e) => RpcResponse::err(req.id, e),
            }
        }
        "save_session" => {
            let params: SaveSessionParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => return RpcResponse::err(req.id, format!("Invalid save_session params: {e}")),
            };
            let root = state.index.read().await.root.clone();
            match state
                .sessions
                .save(root.as_deref(), &params.session_id, &params.title, params.messages)
            {
                Ok(summary) => RpcResponse::ok(req.id, json!({ "summary": summary })),
                Err(e) => RpcResponse::err(req.id, e),
            }
        }
        "delete_session" => {
            let params: SessionIdParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => return RpcResponse::err(req.id, format!("Invalid delete_session params: {e}")),
            };
            let root = state.index.read().await.root.clone();
            match state.sessions.delete(root.as_deref(), &params.session_id) {
                Ok(deleted) => RpcResponse::ok(req.id, json!({ "deleted": deleted })),
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
        "search_symbols" => {
            let params: SearchContextParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => {
                    return RpcResponse::err(req.id, format!("Invalid search_symbols params: {e}"))
                }
            };
            let index = state.index.read().await;
            let hits = index.search_symbols(&params.query, params.limit);
            RpcResponse::ok(req.id, json!({ "symbols": hits }))
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
        "git_context" => {
            let index = state.index.read().await;
            match index.git_context() {
                Ok(ctx) => RpcResponse::ok(req.id, json!({ "context": ctx })),
                Err(e) => RpcResponse::err(req.id, e),
            }
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
        "find_callers" => {
            let params: FindCallersParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => return RpcResponse::err(req.id, format!("Invalid find_callers params: {e}")),
            };
            let index = state.index.read().await;
            let callers = index.find_callers(&params.symbol, params.limit);
            RpcResponse::ok(req.id, json!({ "callers": callers }))
        }
        "file_outline" => {
            let params: FileOutlineParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => return RpcResponse::err(req.id, format!("Invalid file_outline params: {e}")),
            };
            let index = state.index.read().await;
            let chunks = index.file_outline(&params.path);
            RpcResponse::ok(req.id, json!({ "chunks": chunks }))
        }
        "list_memories" => {
            let root = state.index.read().await.root.clone();
            let memories = load_memories(root.as_deref());
            RpcResponse::ok(req.id, json!({ "memories": memories }))
        }
        "save_memory" => {
            let params: SaveMemoryParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => return RpcResponse::err(req.id, format!("Invalid save_memory params: {e}")),
            };
            let root = state.index.read().await.root.clone();
            match save_memory(root.as_deref(), &params.scope, &params.category, &params.content) {
                Ok(entry) => RpcResponse::ok(req.id, json!({ "memory": entry })),
                Err(e) => RpcResponse::err(req.id, e),
            }
        }
        "delete_memory" => {
            let params: DeleteMemoryParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => return RpcResponse::err(req.id, format!("Invalid delete_memory params: {e}")),
            };
            let root = state.index.read().await.root.clone();
            match delete_memory(root.as_deref(), &params.id) {
                Ok(deleted) => RpcResponse::ok(req.id, json!({ "deleted": deleted })),
                Err(e) => RpcResponse::err(req.id, e),
            }
        }
        "list_skills" => {
            let root = state.index.read().await.root.clone();
            let skills = list_skills(root.as_deref());
            RpcResponse::ok(req.id, json!({ "skills": skills }))
        }
        "create_checkpoint" => {
            let params: CreateCheckpointParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => return RpcResponse::err(req.id, format!("Invalid create_checkpoint params: {e}")),
            };
            let Some(root) = state.index.read().await.root.clone() else {
                return RpcResponse::err(req.id, "No workspace root is open.");
            };
            let paths: Vec<PathBuf> = params.paths.iter().map(PathBuf::from).collect();
            match state.checkpoints.create_checkpoint(&root, &params.label, &paths) {
                Ok(summary) => RpcResponse::ok(req.id, json!({ "checkpoint": summary })),
                Err(e) => RpcResponse::err(req.id, e),
            }
        }
        "list_checkpoints" => {
            let Some(root) = state.index.read().await.root.clone() else {
                return RpcResponse::ok(req.id, json!({ "checkpoints": [] }));
            };
            let checkpoints = state.checkpoints.list_checkpoints(&root);
            RpcResponse::ok(req.id, json!({ "checkpoints": checkpoints }))
        }
        "restore_checkpoint" => {
            let params: RestoreCheckpointParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => return RpcResponse::err(req.id, format!("Invalid restore_checkpoint params: {e}")),
            };
            let Some(root) = state.index.read().await.root.clone() else {
                return RpcResponse::err(req.id, "No workspace root is open.");
            };
            match state.checkpoints.restore_checkpoint(&root, &params.checkpoint_id) {
                Ok(res) => {
                    let mut index = state.index.write().await;
                    let _ = index.index_directory(&root);
                    RpcResponse::ok(req.id, json!({ "restored": res }))
                }
                Err(e) => RpcResponse::err(req.id, e),
            }
        }
        "create_worktree" => {
            let params: CreateWorktreeParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => return RpcResponse::err(req.id, format!("Invalid create_worktree params: {e}")),
            };
            let Some(root) = state.index.read().await.root.clone() else {
                return RpcResponse::err(req.id, "No workspace root is open.");
            };
            match create_worktree(&root, &params.task_name) {
                Ok(info) => RpcResponse::ok(req.id, json!({ "worktree": info })),
                Err(e) => RpcResponse::err(req.id, e),
            }
        }
        "list_worktrees" => {
            let Some(root) = state.index.read().await.root.clone() else {
                return RpcResponse::ok(req.id, json!({ "worktrees": [] }));
            };
            match list_worktrees(&root) {
                Ok(items) => RpcResponse::ok(req.id, json!({ "worktrees": items })),
                Err(e) => RpcResponse::err(req.id, e),
            }
        }
        "diff_worktree" => {
            let params: WorktreePathParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => return RpcResponse::err(req.id, format!("Invalid diff_worktree params: {e}")),
            };
            match diff_worktree(&PathBuf::from(&params.worktree_path)) {
                Ok(diff) => RpcResponse::ok(req.id, json!({ "diff": diff })),
                Err(e) => RpcResponse::err(req.id, e),
            }
        }
        "merge_worktree" => {
            let params: MergeWorktreeParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => return RpcResponse::err(req.id, format!("Invalid merge_worktree params: {e}")),
            };
            let Some(root) = state.index.read().await.root.clone() else {
                return RpcResponse::err(req.id, "No workspace root is open.");
            };
            match merge_worktree(&root, &params.branch_name) {
                Ok(out) => RpcResponse::ok(req.id, json!({ "output": out })),
                Err(e) => RpcResponse::err(req.id, e),
            }
        }
        "remove_worktree" => {
            let params: WorktreePathParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => return RpcResponse::err(req.id, format!("Invalid remove_worktree params: {e}")),
            };
            let Some(root) = state.index.read().await.root.clone() else {
                return RpcResponse::err(req.id, "No workspace root is open.");
            };
            match remove_worktree(&root, &PathBuf::from(&params.worktree_path)) {
                Ok(()) => RpcResponse::ok(req.id, json!({ "removed": true })),
                Err(e) => RpcResponse::err(req.id, e),
            }
        }
        "run_pty_command" => {
            let params: RunPtyCommandParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => return RpcResponse::err(req.id, format!("Invalid run_pty_command params: {e}")),
            };
            let cwd = match params.cwd {
                Some(c) if !c.trim().is_empty() => PathBuf::from(c),
                _ => match state.index.read().await.root.clone() {
                    Some(r) => r,
                    None => return RpcResponse::err(req.id, "No workspace root is open."),
                },
            };
            let tx = out_tx.clone();
            let req_id = req.id;
            match run_pty_command(&params.command, &cwd, params.timeout_secs.unwrap_or(60), move |chunk| {
                emit_event(&tx, req_id, "pty_chunk", json!({ "text": chunk }));
            })
            .await
            {
                Ok((exit_code, output)) => {
                    RpcResponse::ok(req.id, json!({ "exitCode": exit_code, "output": output }))
                }
                Err(e) => RpcResponse::err(req.id, e),
            }
        }
        "start_background_process" => {
            let params: StartBgProcessParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => return RpcResponse::err(req.id, format!("Invalid start_background_process params: {e}")),
            };
            let cwd = match params.cwd {
                Some(c) if !c.trim().is_empty() => PathBuf::from(c),
                _ => match state.index.read().await.root.clone() {
                    Some(r) => r,
                    None => return RpcResponse::err(req.id, "No workspace root is open."),
                },
            };
            match state.pty_manager.start_background(&params.command, &cwd) {
                Ok(info) => RpcResponse::ok(req.id, json!({ "process": info })),
                Err(e) => RpcResponse::err(req.id, e),
            }
        }
        "read_background_process" => {
            let params: BgProcessIdParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => return RpcResponse::err(req.id, format!("Invalid read_background_process params: {e}")),
            };
            match state.pty_manager.read_output(&params.proc_id) {
                Ok(out) => RpcResponse::ok(req.id, json!({ "process": out })),
                Err(e) => RpcResponse::err(req.id, e),
            }
        }
        "write_background_process" => {
            let params: WriteBgProcessParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => return RpcResponse::err(req.id, format!("Invalid write_background_process params: {e}")),
            };
            match state.pty_manager.write_input(&params.proc_id, &params.input) {
                Ok(()) => RpcResponse::ok(req.id, json!({ "written": true })),
                Err(e) => RpcResponse::err(req.id, e),
            }
        }
        "stop_background_process" => {
            let params: BgProcessIdParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => return RpcResponse::err(req.id, format!("Invalid stop_background_process params: {e}")),
            };
            match state.pty_manager.stop_process(&params.proc_id) {
                Ok(stopped) => RpcResponse::ok(req.id, json!({ "stopped": stopped })),
                Err(e) => RpcResponse::err(req.id, e),
            }
        }
        "list_background_processes" => {
            let procs = state.pty_manager.list_processes();
            RpcResponse::ok(req.id, json!({ "processes": procs }))
        }
        "detect_verify_commands" => {
            let Some(root) = state.index.read().await.root.clone() else {
                return RpcResponse::ok(req.id, json!({ "commands": [] }));
            };
            let commands = detect_verify_commands(&root);
            RpcResponse::ok(req.id, json!({ "commands": commands }))
        }
        "validate_syntax" => {
            let params: ValidateSyntaxParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => return RpcResponse::err(req.id, format!("Invalid validate_syntax params: {e}")),
            };
            let validation = validate_edit_syntax(&params.path, &params.before, &params.after);
            RpcResponse::ok(req.id, json!({ "validation": validation }))
        }
        "repo_map" => {
            let params: RepoMapParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => return RpcResponse::err(req.id, format!("Invalid repo_map params: {e}")),
            };
            let files: Vec<RepoFileInput> = {
                let index = state.index.read().await;
                index
                    .files
                    .iter()
                    .take(800)
                    .map(|f| RepoFileInput {
                        path: f.rel_path.clone(),
                        content: f.content.clone(),
                    })
                    .collect()
            };
            let seed = RepoMapSeed {
                active_files: params.active_files,
                dirty_files: params.dirty_files,
                query_terms: params.query_terms,
            };
            let map = build_pagerank_repo_map(&files, &seed, params.token_budget_chars);
            RpcResponse::ok(req.id, json!({ "repoMap": map }))
        }
        "create_cow_clone" => {
            let params: CreateCowCloneParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => return RpcResponse::err(req.id, format!("Invalid create_cow_clone params: {e}")),
            };
            let Some(root) = state.index.read().await.root.clone() else {
                return RpcResponse::err(req.id, "No workspace root is open.");
            };
            match state.cow_clones.create_clone(&root, &params.task_label) {
                Ok(meta) => RpcResponse::ok(req.id, json!({ "clone": meta })),
                Err(e) => RpcResponse::err(req.id, e),
            }
        }
        "list_cow_clones" => {
            let clones = state.cow_clones.list_clones();
            RpcResponse::ok(req.id, json!({ "clones": clones }))
        }
        "diff_cow_clone" => {
            let params: CowCloneIdParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => return RpcResponse::err(req.id, format!("Invalid diff_cow_clone params: {e}")),
            };
            match state.cow_clones.diff_clone(&params.clone_id) {
                Ok(diffs) => RpcResponse::ok(req.id, json!({ "diffs": diffs })),
                Err(e) => RpcResponse::err(req.id, e),
            }
        }
        "apply_cow_clone" => {
            let params: CowCloneIdParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => return RpcResponse::err(req.id, format!("Invalid apply_cow_clone params: {e}")),
            };
            match state.cow_clones.apply_clone_back(&params.clone_id, None) {
                Ok(applied) => {
                    if let Some(root) = state.index.read().await.root.clone() {
                        let mut index = state.index.write().await;
                        let _ = index.index_directory(&root);
                    }
                    RpcResponse::ok(req.id, json!({ "appliedFiles": applied }))
                }
                Err(e) => RpcResponse::err(req.id, e),
            }
        }
        "destroy_cow_clone" => {
            let params: CowCloneIdParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => return RpcResponse::err(req.id, format!("Invalid destroy_cow_clone params: {e}")),
            };
            match state.cow_clones.destroy_clone(&params.clone_id) {
                Ok(destroyed) => RpcResponse::ok(req.id, json!({ "destroyed": destroyed })),
                Err(e) => RpcResponse::err(req.id, e),
            }
        }
        "classify_command" => {
            let cmd = req
                .params
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let risk = classify_command_risk(cmd);
            let profile = *state.permission_profile.read().await;
            RpcResponse::ok(req.id, json!({ "risk": risk, "profile": profile }))
        }
        "run_sandboxed_command" => {
            let params: RunSandboxedCommandParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => return RpcResponse::err(req.id, format!("Invalid run_sandboxed_command params: {e}")),
            };
            let cwd = match params.cwd {
                Some(c) if !c.trim().is_empty() => PathBuf::from(c),
                _ => match state.index.read().await.root.clone() {
                    Some(r) => r,
                    None => return RpcResponse::err(req.id, "No workspace root is open."),
                },
            };
            let profile = match params.profile.as_deref().and_then(PermissionProfile::from_str_name) {
                Some(p) => p,
                None => *state.permission_profile.read().await,
            };
            let allow_network = match params.allow_network {
                Some(b) => b,
                None => *state.sandbox_allow_network.read().await,
            };
            let timeout = params.timeout_secs.unwrap_or(60);
            match run_sandboxed_command_async(&params.command, &cwd, profile, allow_network, timeout).await {
                Ok(out) => RpcResponse::ok(req.id, json!({ "result": out })),
                Err(e) => RpcResponse::err(req.id, e),
            }
        }
        "plan_swarm_waves" => {
            let tasks_val = req.params.get("tasks").cloned().unwrap_or(Value::Array(vec![]));
            let tasks: Vec<SwarmTaskSpec> = match serde_json::from_value(tasks_val) {
                Ok(t) => t,
                Err(e) => return RpcResponse::err(req.id, format!("Invalid swarm tasks: {e}")),
            };
            match compute_execution_waves(&tasks) {
                Ok(waves) => RpcResponse::ok(req.id, json!({ "waves": waves, "waveCount": waves.len() })),
                Err(e) => RpcResponse::err(req.id, e),
            }
        }
        "run_swarm" => {
            let goal = req
                .params
                .get("goal")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let tasks_override: Option<Vec<SwarmTaskSpec>> = req
                .params
                .get("tasks")
                .and_then(|v| serde_json::from_value(v.clone()).ok());
            let max_parallel = req
                .params
                .get("maxParallelWorkers")
                .and_then(|v| v.as_u64())
                .unwrap_or(3) as usize;
            let max_turns = req
                .params
                .get("maxTurns")
                .and_then(|v| v.as_u64())
                .unwrap_or(8) as usize;
            let allow_net = match req.params.get("allowNetwork").and_then(|v| v.as_bool()) {
                Some(b) => b,
                None => *state.sandbox_allow_network.read().await,
            };
            let model_override = req
                .params
                .get("model")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let root = match req
                .params
                .get("workspaceRoot")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(PathBuf::from)
            {
                Some(r) => r,
                None => match state.index.read().await.root.clone() {
                    Some(r) => r,
                    None => return RpcResponse::err(req.id, "No workspace root is open."),
                },
            };

            let files: Vec<RepoFileInput> = {
                let index = state.index.read().await;
                index
                    .files
                    .iter()
                    .take(400)
                    .map(|f| RepoFileInput {
                        path: f.rel_path.clone(),
                        content: f.content.clone(),
                    })
                    .collect()
            };
            let repo_map = build_pagerank_repo_map(&files, &RepoMapSeed::default(), 4_000);
            let router = state.smart_router().await;
            let keys = state.provider_keys().await;
            let tx = out_tx.clone();
            let req_id = req.id;

            match run_swarm_mission(
                &goal,
                tasks_override,
                &repo_map,
                max_parallel,
                max_turns,
                allow_net,
                model_override.as_deref(),
                &root,
                &state.cow_clones,
                &router,
                &keys,
                Arc::new(move |ev| {
                    if let Ok(val) = serde_json::to_value(ev) {
                        emit_event(&tx, req_id, "swarm_event", val);
                    }
                }),
            )
            .await
            {
                Ok(report) => {
                    let val = serde_json::to_value(&report).unwrap_or_else(|_| json!({}));
                    RpcResponse::ok(req.id, val)
                }
                Err(e) => RpcResponse::err(req.id, e),
            }
        }
        "merge_swarm" => {
            let nodes_val = req.params.get("nodes").cloned().unwrap_or(Value::Array(vec![]));
            let nodes: Vec<SwarmNodeState> = match serde_json::from_value(nodes_val) {
                Ok(n) => n,
                Err(e) => return RpcResponse::err(req.id, format!("Invalid swarm nodes: {e}")),
            };
            let staging_only = req
                .params
                .get("stagingOnly")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let root = match req
                .params
                .get("workspaceRoot")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(PathBuf::from)
            {
                Some(r) => r,
                None => match state.index.read().await.root.clone() {
                    Some(r) => r,
                    None => return RpcResponse::err(req.id, "No workspace root is open."),
                },
            };

            let res = if staging_only {
                merge_worker_clones_to_staging(&nodes, &root, &state.cow_clones)
            } else {
                let merged = merge_worker_clones_to_workspace(&nodes, &root, &state.cow_clones);
                if merged.is_ok() {
                    let mut idx = state.index.write().await;
                    let _ = idx.index_directory(&root);
                }
                merged
            };
            match res {
                Ok(report) => RpcResponse::ok(
                    req.id,
                    serde_json::to_value(&report).unwrap_or_else(|_| json!({})),
                ),
                Err(e) => RpcResponse::err(req.id, e),
            }
        }
        "cleanup_swarm" => {
            let clone_ids: Vec<String> = req
                .params
                .get("cloneIds")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            let destroyed = cleanup_swarm_clones(&clone_ids, &state.cow_clones);
            RpcResponse::ok(req.id, json!({ "destroyedCloneIds": destroyed }))
        }
        "run_arena" => {
            let prompt = req
                .params
                .get("prompt")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let models: Vec<String> = req
                .params
                .get("models")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_else(|| vec![DEFAULT_GEMINI_MODEL.to_string()]);
            let allow_net = *state.sandbox_allow_network.read().await;
            let root = match req
                .params
                .get("workspaceRoot")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(PathBuf::from)
            {
                Some(r) => r,
                None => match state.index.read().await.root.clone() {
                    Some(r) => r,
                    None => return RpcResponse::err(req.id, "No workspace root is open."),
                },
            };
            let router = state.smart_router().await;
            let keys = state.provider_keys().await;
            let tx = out_tx.clone();
            let req_id = req.id;

            let candidates = run_model_arena_with_events(
                &prompt,
                models,
                &root,
                &state.cow_clones,
                &router,
                &keys,
                allow_net,
                Arc::new(move |ev| {
                    if let Ok(val) = serde_json::to_value(ev) {
                        emit_event(&tx, req_id, "swarm_event", val);
                    }
                }),
            )
            .await;
            RpcResponse::ok(req.id, json!({ "candidates": candidates }))
        }
        "browser_probe" => {
            let chromium = discover_chromium_binary().ok();
            let ffmpeg = discover_ffmpeg_binary().ok();
            RpcResponse::ok(
                req.id,
                json!({
                    "chromiumPath": chromium.as_ref().map(|p| p.display().to_string()),
                    "ffmpegPath": ffmpeg.as_ref().map(|p| p.display().to_string()),
                    "chromiumFound": chromium.is_some(),
                    "ffmpegFound": ffmpeg.is_some(),
                    "walkthroughRecordingReady": chromium.is_some() && ffmpeg.is_some(),
                }),
            )
        }
        "browser_navigate"
        | "browser_snapshot"
        | "browser_act"
        | "browser_diagnostics"
        | "browser_clear_diagnostics"
        | "browser_export_pdf"
        | "browser_record_walkthrough"
        | "browser_close" => match state.browser.handle_rpc(&req.method, req.params).await {
            Ok(val) => RpcResponse::ok(req.id, val),
            Err(e) => RpcResponse::err(req.id, e),
        },
        "create_presentation"
        | "render_explainer_video"
        | "generate_veo_video"
        | "generate_image_asset"
        | "create_spreadsheet"
        | "create_document" => {
            let root = match req
                .params
                .get("workspaceRoot")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(PathBuf::from)
            {
                Some(r) => r,
                None => match state.index.read().await.root.clone() {
                    Some(r) => r,
                    None => return RpcResponse::err(req.id, "No workspace root is open."),
                },
            };
            let api_key = state.api_key.read().await.clone();
            match dispatch_media_studio_rpc(
                &req.method,
                &req.params,
                &root,
                &state.http,
                api_key.as_deref(),
            )
            .await
            {
                Some(Ok(val)) => RpcResponse::ok(req.id, val),
                Some(Err(e)) => RpcResponse::err(req.id, e),
                None => RpcResponse::err(req.id, format!("Unhandled media RPC: {}", req.method)),
            }
        }
        "web_search" => {
            let query = req.params.get("query").and_then(|v| v.as_str()).unwrap_or("");
            let limit = req.params.get("limit").and_then(|v| v.as_u64()).unwrap_or(6) as usize;
            let api_key = state.api_key.read().await.clone();
            let model = state.model.read().await.clone();
            match web_search(&state.http, query, limit, api_key.as_deref(), Some(&model)).await {
                Ok(res) => RpcResponse::ok(req.id, serde_json::to_value(res).unwrap_or_else(|_| json!({}))),
                Err(e) => RpcResponse::err(req.id, e),
            }
        }
        "read_url_markdown" => {
            let url = req.params.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let max_chars = req
                .params
                .get("maxChars")
                .and_then(|v| v.as_u64())
                .unwrap_or(24_000) as usize;
            let allow_localhost = req
                .params
                .get("allowLocalhost")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            match fetch_url_as_markdown(&state.http, url, max_chars, allow_localhost).await {
                Ok(res) => RpcResponse::ok(req.id, serde_json::to_value(res).unwrap_or_else(|_| json!({}))),
                Err(e) => RpcResponse::err(req.id, e),
            }
        }
        "index_docs_source" => {
            let name = req.params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let start_url = req.params.get("startUrl").and_then(|v| v.as_str()).unwrap_or("");
            let max_pages = req
                .params
                .get("maxPages")
                .and_then(|v| v.as_u64())
                .unwrap_or(25) as usize;
            let allow_localhost = req
                .params
                .get("allowLocalhost")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            match state
                .docs_index
                .crawl_and_index(&state.http, name, start_url, max_pages, allow_localhost)
                .await
            {
                Ok(summary) => RpcResponse::ok(req.id, json!({ "source": summary })),
                Err(e) => RpcResponse::err(req.id, e),
            }
        }
        "index_docs_pages" => {
            let name = req.params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let root_url = req
                .params
                .get("rootUrl")
                .and_then(|v| v.as_str())
                .unwrap_or("https://local.docs/");
            let raw_pages = req
                .params
                .get("pages")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let mut pages: Vec<(String, String, String)> = Vec::new();
            for p in raw_pages {
                let url = p.get("url").and_then(|v| v.as_str()).unwrap_or(root_url).to_string();
                let title = p.get("title").and_then(|v| v.as_str()).unwrap_or(name).to_string();
                let markdown = p.get("markdown").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if !markdown.trim().is_empty() {
                    pages.push((url, title, markdown));
                }
            }
            match state.docs_index.index_pages(name, root_url, &pages, false) {
                Ok(summary) => RpcResponse::ok(req.id, json!({ "source": summary })),
                Err(e) => RpcResponse::err(req.id, e),
            }
        }
        "search_docs" => {
            let query = req.params.get("query").and_then(|v| v.as_str()).unwrap_or("");
            let source_filter = req.params.get("source").and_then(|v| v.as_str());
            let limit = req.params.get("limit").and_then(|v| v.as_u64()).unwrap_or(6) as usize;
            match state.docs_index.search_docs(query, source_filter, limit) {
                Ok(hits) => RpcResponse::ok(req.id, json!({ "hits": hits })),
                Err(e) => RpcResponse::err(req.id, e),
            }
        }
        "list_docs_sources" => match state.docs_index.list_sources() {
            Ok(sources) => RpcResponse::ok(req.id, json!({ "sources": sources })),
            Err(e) => RpcResponse::err(req.id, e),
        },
        "remove_docs_source" => {
            let name = req.params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            match state.docs_index.remove_source(name) {
                Ok(removed) => RpcResponse::ok(req.id, json!({ "removed": removed })),
                Err(e) => RpcResponse::err(req.id, e),
            }
        }
        "list_mcp_catalog" => {
            let presets = builtin_mcp_catalog();
            RpcResponse::ok(req.id, json!({ "presets": presets }))
        }
        "install_mcp_preset" => {
            let preset_id = req.params.get("presetId").and_then(|v| v.as_str()).unwrap_or("");
            let scope = req
                .params
                .get("scope")
                .and_then(|v| v.as_str())
                .unwrap_or("workspace");
            let env_values: HashMap<String, String> = req
                .params
                .get("envValues")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            let extra_args: Vec<String> = req
                .params
                .get("extraArgs")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            let ws_root = req
                .params
                .get("workspaceRoot")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(PathBuf::from)
                .or_else(|| state.index.try_read().ok().and_then(|idx| idx.root.clone()));

            let target_path = if scope == "workspace" {
                match ws_root {
                    Some(root) => root.join(".rursor").join("mcp.json"),
                    None => return RpcResponse::err(req.id, "No workspace root open for workspace MCP config."),
                }
            } else {
                std::env::var("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join(".rursor")
                    .join("mcp.json")
            };

            match install_mcp_preset(&target_path, preset_id, env_values, extra_args) {
                Ok(val) => RpcResponse::ok(req.id, val),
                Err(e) => RpcResponse::err(req.id, e),
            }
        }
        "run_parallel_subagents" => {
            let params: RunParallelSubagentsParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => return RpcResponse::err(req.id, format!("Invalid run_parallel_subagents params: {e}")),
            };
            let tasks: Vec<SubagentTask> = match serde_json::from_value(params.tasks) {
                Ok(t) => t,
                Err(e) => return RpcResponse::err(req.id, format!("Invalid tasks array: {e}")),
            };
            let Some(api_key) = state.api_key.read().await.clone() else {
                return RpcResponse::err(req.id, "Gemini API key is not configured.");
            };
            let model = params.model.unwrap_or_else(|| DEFAULT_GEMINI_MODEL.to_string());
            let rules = state.workspace_rules.read().await.clone();
            let req_id = req.id;
            let results = run_parallel_subagents(
                &state.gemini,
                &api_key,
                &model,
                rules.as_deref(),
                tasks,
                |task_id, role, status, output| {
                    emit_event(
                        out_tx,
                        req_id,
                        "subagent_event",
                        json!({
                            "taskId": task_id,
                            "role": role,
                            "status": status,
                            "output": output,
                        }),
                    );
                },
            )
            .await;
            RpcResponse::ok(req.id, json!({ "results": results }))
        }
        "next_edit_predict" => handle_next_edit_predict(req, state).await,
        "inline_edit" => handle_inline_edit(req, state, out_tx).await,
        "inline_complete" => handle_inline_complete(req, state).await,
        "agent_chat" => handle_agent_chat(req, state, out_tx).await,
        other => RpcResponse::err(req.id, format!("Unknown method: {other}")),
    }
}

async fn handle_next_edit_predict(req: RpcRequest, state: Arc<AppState>) -> RpcResponse {
    let params: NextEditPredictParams = match serde_json::from_value(req.params) {
        Ok(p) => p,
        Err(e) => return RpcResponse::err(req.id, format!("Invalid next_edit_predict params: {e}")),
    };

    let related_sites = {
        let index = state.index.read().await;
        let files: Vec<(String, String)> = index
            .files
            .iter()
            .take(500)
            .map(|f| (f.rel_path.clone(), f.content.clone()))
            .collect();
        let hint = params
            .recent_edits
            .first()
            .map(String::as_str)
            .unwrap_or(params.focus_block.as_str());
        find_related_edit_sites(hint, &params.file_path, &files, 6)
    };

    let (system_prompt, mut user_prompt) = build_nes_prompt(
        &params.file_path,
        &params.language_id,
        &params.window_before,
        &params.focus_block,
        &params.window_after,
        params.focus_start_line.max(1),
        &params.recent_edits,
        &related_sites,
    );
    if let Some(intent) = params.developer_intent.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        user_prompt.push_str(&format!("\n\n{intent}"));
    }

    let focus_lines = params.focus_block.lines().count().max(1);
    let default_start = params.focus_start_line.max(1);
    let default_end = default_start + focus_lines - 1;

    let backend = state.autocomplete_backend.read().await.clone();
    let local_endpoint = state.local_endpoint.read().await.clone();
    let local_model = state.local_model.read().await.clone();

    if backend == "ollama" || backend == "mlx" || backend == "auto" {
        let try_backend = if backend == "auto" {
            if local_endpoint.contains("11434") {
                "ollama"
            } else {
                "mlx"
            }
        } else {
            backend.as_str()
        };

        if let Ok(raw) = state
            .local_llm
            .complete_fim(try_backend, &local_endpoint, &local_model, &system_prompt, &user_prompt)
            .await
        {
            if let Some(pred) = parse_nes_response(&raw, &params.file_path, default_start, default_end)
            {
                return RpcResponse::ok(req.id, json!({ "prediction": pred, "backend": try_backend }));
            }
        }
    }

    let Some(api_key) = state.api_key.read().await.clone() else {
        return RpcResponse::ok(req.id, json!({ "prediction": Value::Null }));
    };
    let model = match params.model {
        Some(m) if !m.trim().is_empty() => m,
        _ => state.autocomplete_model.read().await.clone(),
    };

    let messages = vec![ChatMessage {
        role: "user".to_string(),
        content: user_prompt,
        images: vec![],
    }];

    match state
        .gemini
        .stream_chat(&api_key, &model, Some(&system_prompt), &messages, 0.1, Some(320), |_| {})
        .await
    {
        Ok(raw) => {
            let pred = parse_nes_response(&raw, &params.file_path, default_start, default_end);
            RpcResponse::ok(req.id, json!({ "prediction": pred, "backend": "gemini" }))
        }
        Err(e) => RpcResponse::err(req.id, e),
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

    let keys = state.provider_keys().await;
    let default_model = state.model.read().await.clone();
    let requested_model = params
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .or(Some(default_model.as_str()));
    let local_backend = state.autocomplete_backend.read().await.clone();
    let local_model = state.local_model.read().await.clone();
    let route = route_task(
        TaskKind::InlineEdit,
        &params.instruction,
        requested_model,
        &local_backend,
        &local_model,
        &keys,
    );

    let mut diag_block = String::new();
    if !params.diagnostics.is_empty() {
        diag_block.push_str("\n\n<diagnostics>\n");
        for d in &params.diagnostics {
            diag_block.push_str(&format!("[{}] line {}: {}\n", d.severity, d.line, d.message));
        }
        diag_block.push_str("</diagnostics>");
    }
    if let Some(lsp_ctx) = params.lsp_context.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        diag_block.push_str(&format!("\n\n{lsp_ctx}"));
    }
    if let Some(intent) = params.developer_intent.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        diag_block.push_str(&format!("\n\n{intent}"));
    }

    let rules_section = state
        .workspace_rules
        .read()
        .await
        .clone()
        .map(|r| format!("\n\nWorkspace Rules:\n{r}"))
        .unwrap_or_default();

    let system_prompt = format!(
        "You are Rursor's inline code editor. Output ONLY the exact replacement code for the <selection> region—no markdown code fences, no explanations, no surrounding context, and preserve indentation.{rules_section}"
    );
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
        images: vec![],
    }];

    let req_id = req.id;
    let res = match route.provider {
        ProviderKind::Anthropic => {
            let Some(anth_key) = keys.anthropic else {
                return RpcResponse::err(req.id, "Anthropic API key is not configured.");
            };
            stream_anthropic_chat(
                &state.http,
                &anth_key,
                &route.model,
                Some(&system_prompt),
                &messages,
                0.2,
                None,
                None,
                |delta| {
                    emit_event(out_tx, req_id, "stream_chunk", json!({ "text": delta }));
                },
            )
            .await
        }
        ProviderKind::OpenAi => {
            let Some(oai_key) = keys.openai else {
                return RpcResponse::err(req.id, "OpenAI API key is not configured.");
            };
            stream_openai_chat(
                &state.http,
                &oai_key,
                &route.model,
                Some(&system_prompt),
                &messages,
                0.2,
                None,
                |delta| {
                    emit_event(out_tx, req_id, "stream_chunk", json!({ "text": delta }));
                },
            )
            .await
        }
        ProviderKind::Gemini | ProviderKind::LocalLlm => {
            let Some(api_key) = keys.gemini.as_deref() else {
                return RpcResponse::err(
                    req.id,
                    "Gemini API key is not configured. Run 'Rursor: Set Gemini API Key'.",
                );
            };
            state
                .gemini
                .stream_chat(api_key, &route.model, Some(&system_prompt), &messages, 0.2, None, |delta| {
                    emit_event(out_tx, req_id, "stream_chunk", json!({ "text": delta }));
                })
                .await
        }
    };

    match res {
        Ok(full) => {
            let mut cleaned = strip_code_fences(&full);
            let mut syntax_repaired = false;

            if let (Some(before_full), Some(start_off), Some(end_off)) = (
                params.before_file_content.as_deref(),
                params.selection_start_offset,
                params.selection_end_offset,
            ) {
                if start_off <= end_off
                    && end_off <= before_full.len()
                    && before_full.is_char_boundary(start_off)
                    && before_full.is_char_boundary(end_off)
                    && is_syntax_gate_supported(&params.file_path)
                {
                    let candidate_full = format!(
                        "{}{}{}",
                        &before_full[..start_off],
                        cleaned,
                        &before_full[end_off..]
                    );
                    let gate = validate_edit_syntax(&params.file_path, before_full, &candidate_full);
                    if !gate.valid {
                        if let Some(api_key) = keys.gemini.as_deref() {
                            let repair_msgs = vec![ChatMessage {
                                role: "user".to_string(),
                                content: format!(
                                    "{}\n\nYour previous replacement introduced syntax errors:\n{}\n\nPrevious replacement:\n{}\n\nOutput ONLY the fixed replacement for <selection> with valid syntax and zero markdown fences.",
                                    messages[0].content, gate.message, cleaned
                                ),
                                images: vec![],
                            }];
                            if let Ok(repaired_raw) = state
                                .gemini
                                .stream_chat(
                                    api_key,
                                    &route.model,
                                    Some(&system_prompt),
                                    &repair_msgs,
                                    0.1,
                                    None,
                                    |_| {},
                                )
                                .await
                            {
                                let repaired_clean = strip_code_fences(&repaired_raw);
                                let repaired_full = format!(
                                    "{}{}{}",
                                    &before_full[..start_off],
                                    repaired_clean,
                                    &before_full[end_off..]
                                );
                                if validate_edit_syntax(&params.file_path, before_full, &repaired_full).valid {
                                    cleaned = repaired_clean;
                                    syntax_repaired = true;
                                }
                            }
                        }
                    }
                }
            }

            RpcResponse::ok(
                req.id,
                json!({
                    "replacement": cleaned,
                    "syntaxRepaired": syntax_repaired,
                }),
            )
        }
        Err(e) => RpcResponse::err(req.id, e),
    }
}

async fn handle_inline_complete(req: RpcRequest, state: Arc<AppState>) -> RpcResponse {
    let params: InlineCompleteParams = match serde_json::from_value(req.params) {
        Ok(p) => p,
        Err(e) => return RpcResponse::err(req.id, format!("Invalid inline_complete params: {e}")),
    };

    let mut recent_block = String::new();
    for rf in params.recent_files.iter().take(2) {
        recent_block.push_str(&format!("\n--- Related file: {} ---\n{}\n", rf.path, rf.content));
    }
    if let Some(hint) = &params.recent_edit_hint {
        if !hint.trim().is_empty() {
            recent_block.push_str(&format!("\n--- Recent edit ---\n{hint}\n"));
        }
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

    let backend = state.autocomplete_backend.read().await.clone();
    let local_endpoint = state.local_endpoint.read().await.clone();
    let local_model = state.local_model.read().await.clone();

    if backend == "ollama" || backend == "mlx" || backend == "auto" {
        let try_backend = if backend == "auto" {
            if local_endpoint.contains("11434") {
                "ollama"
            } else {
                "mlx"
            }
        } else {
            backend.as_str()
        };

        match state
            .local_llm
            .complete_fim(try_backend, &local_endpoint, &local_model, system_prompt, &user_prompt)
            .await
        {
            Ok(raw) => {
                let completion = clean_fim_completion(&raw, &params.prefix, &params.suffix);
                return RpcResponse::ok(
                    req.id,
                    json!({ "completion": completion, "backend": try_backend }),
                );
            }
            Err(err) if backend != "auto" => {
                return RpcResponse::err(req.id, err);
            }
            Err(_) => {
                // In 'auto' mode, fall back silently to Gemini Cloud API.
            }
        }
    }

    let Some(api_key) = state.api_key.read().await.clone() else {
        return RpcResponse::ok(req.id, json!({ "completion": "" }));
    };

    let model = match params.model {
        Some(m) if !m.trim().is_empty() => m,
        _ => state.autocomplete_model.read().await.clone(),
    };

    let messages = vec![ChatMessage {
        role: "user".to_string(),
        content: user_prompt,
        images: vec![],
    }];

    let res = state
        .gemini
        .stream_chat(&api_key, &model, Some(system_prompt), &messages, 0.1, Some(180), |_| {})
        .await;

    match res {
        Ok(raw) => {
            let completion = clean_fim_completion(&raw, &params.prefix, &params.suffix);
            RpcResponse::ok(req.id, json!({ "completion": completion, "backend": "gemini" }))
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

    let req_id = req.id;
    let workspace_root = state.index.read().await.root.clone();
    let original_messages = params.messages.clone();
    let mut messages = params.messages;

    // Expand `/slash` skill commands if the latest user message starts with `/`
    if let Some(last_user) = messages.iter_mut().rev().find(|m| m.role == "user") {
        if let Some((skill, expanded)) =
            resolve_slash_command(workspace_root.as_deref(), &last_user.content)
        {
            emit_event(
                out_tx,
                req_id,
                "tool_event",
                json!({
                    "callId": format!("skill_{}", skill.name),
                    "phase": "done",
                    "name": format!("/{}", skill.name),
                    "ok": true,
                    "output": { "source": skill.source, "description": skill.description },
                }),
            );
            last_user.content = expanded;
        }
    }

    let prompt_hint = messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.content.as_str())
        .unwrap_or("");

    let keys = state.provider_keys().await;
    let default_model = state.model.read().await.clone();
    let requested_model = params
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .or(Some(default_model.as_str()));
    let local_backend = state.autocomplete_backend.read().await.clone();
    let local_model = state.local_model.read().await.clone();
    let route = route_task(
        TaskKind::AgentFast,
        prompt_hint,
        requested_model,
        &local_backend,
        &local_model,
        &keys,
    );
    let mut configured_thinking = params
        .thinking_budget
        .or(*state.thinking_budget.read().await)
        .or(route.thinking_budget);
    // Default a modest Gemini thinking budget so the agent UI can animate Thought tokens
    // (unless Ask mode, or the user/router already chose a budget).
    if configured_thinking.is_none()
        && route.provider == ProviderKind::Gemini
        && !params.mode.eq_ignore_ascii_case("ask")
    {
        configured_thinking = Some(2048);
    }

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

    if params.include_git_diff {
        if let Ok(git_ctx) = state.index.read().await.git_context() {
            context_blocks.push_str(&format!("\n{git_ctx}\n"));
        }
    }

    if params.use_workspace_search {
        if let Some(last_user) = messages.iter().rev().find(|m| m.role == "user") {
            let hits = state.index.read().await.search(&last_user.content, 5);
            for hit in hits {
                context_blocks.push_str(&format!(
                    "\n--- Workspace Match: {} (score {}) ---\n{}\n",
                    hit.path, hit.score, hit.preview
                ));
            }

            let repo_files: Vec<RepoFileInput> = {
                let index = state.index.read().await;
                index
                    .files
                    .iter()
                    .take(500)
                    .map(|f| RepoFileInput {
                        path: f.rel_path.clone(),
                        content: f.content.clone(),
                    })
                    .collect()
            };
            if !repo_files.is_empty() {
                let active_files: Vec<String> =
                    params.context_files.iter().map(|c| c.path.clone()).collect();
                let query_terms: Vec<String> = last_user
                    .content
                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                    .filter(|w| w.len() >= 3)
                    .take(16)
                    .map(str::to_string)
                    .collect();
                let seed = RepoMapSeed {
                    active_files,
                    dirty_files: Vec::new(),
                    query_terms,
                };
                let repo_map = build_pagerank_repo_map(&repo_files, &seed, 5_000);
                if !repo_map.is_empty() {
                    context_blocks.push_str(&format!("\n<repo_map>\n{repo_map}\n</repo_map>\n"));
                }
            }
        }
    }

    if let Some(intent) = params.developer_intent.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        context_blocks.push_str(&format!("\n{intent}\n"));
    }

    if !context_blocks.is_empty() {
        if let Some(last) = messages.last_mut() {
            last.content = format!("Workspace Context:\n{context_blocks}\n\nRequest:\n{}", last.content);
        }
    }

    let rules_block = state
        .workspace_rules
        .read()
        .await
        .clone()
        .map(|r| format!("\n\nProject Rules (must follow):\n{r}"))
        .unwrap_or_default();

    let memory_block = format_memory_prompt(workspace_root.as_deref())
        .map(|m| format!("\n\n{m}"))
        .unwrap_or_default();

    // Compact older conversation turns automatically when approaching context limits.
    if estimate_messages_tokens(&messages) > COMPACTION_TOKEN_THRESHOLD {
        if let Some(api_key) = keys.gemini.as_deref() {
            if let Ok(compacted) = state
                .gemini
                .compact_messages(api_key, "gemini-3.5-flash-lite", &messages, 6)
                .await
            {
                emit_event(
                    out_tx,
                    req_id,
                    "tool_event",
                    json!({
                        "callId": format!("compact_{req_id}"),
                        "phase": "done",
                        "name": "compact_history",
                        "ok": true,
                        "output": { "keptMessages": compacted.len() },
                    }),
                );
                messages = compacted;
            }
        }
    }

    if params.mode == "ask" {
        let system_prompt = format!(
            "You are Rursor, a concise, accurate AI coding assistant inside a macOS code editor. Reference exact file paths and symbols from context when available, and provide clean, directly applicable code blocks.{rules_block}{memory_block}"
        );
        let res = match route.provider {
            ProviderKind::Anthropic => {
                let Some(anth_key) = keys.anthropic else {
                    return RpcResponse::err(req.id, "Anthropic API key is not configured.");
                };
                stream_anthropic_chat(
                    &state.http,
                    &anth_key,
                    &route.model,
                    Some(&system_prompt),
                    &messages,
                    0.3,
                    None,
                    configured_thinking,
                    |delta| {
                        emit_event(out_tx, req_id, "stream_chunk", json!({ "text": delta }));
                    },
                )
                .await
            }
            ProviderKind::OpenAi => {
                let Some(oai_key) = keys.openai else {
                    return RpcResponse::err(req.id, "OpenAI API key is not configured.");
                };
                stream_openai_chat(
                    &state.http,
                    &oai_key,
                    &route.model,
                    Some(&system_prompt),
                    &messages,
                    0.3,
                    None,
                    |delta| {
                        emit_event(out_tx, req_id, "stream_chunk", json!({ "text": delta }));
                    },
                )
                .await
            }
            ProviderKind::Gemini | ProviderKind::LocalLlm => {
                let Some(api_key) = keys.gemini else {
                    return RpcResponse::err(
                        req.id,
                        "Gemini API key is not configured. Run 'Rursor: Set Gemini API Key'.",
                    );
                };
                state
                    .gemini
                    .stream_chat(&api_key, &route.model, Some(&system_prompt), &messages, 0.3, None, |delta| {
                        emit_event(out_tx, req_id, "stream_chunk", json!({ "text": delta }));
                    })
                    .await
            }
        };

        return match res {
            Ok(full) => {
                persist_session_turn(&state, params.session_id.as_deref(), original_messages, &full).await;
                RpcResponse::ok(req.id, json!({ "text": full, "route": route }))
            }
            Err(e) => RpcResponse::err(req.id, e),
        };
    }

    // In Agent mode, support Anthropic/OpenAI direct fallback if no Gemini key is set or user forced Claude/GPT.
    if route.provider == ProviderKind::Anthropic && keys.has_anthropic() && !keys.has_gemini() {
        let system_prompt = format!(
            "You are Rursor Agent (text fallback mode — Gemini key not configured for tool calls). Provide exact, actionable code changes.{rules_block}{memory_block}"
        );
        let res = stream_anthropic_chat(
            &state.http,
            keys.anthropic.as_deref().unwrap_or_default(),
            &route.model,
            Some(&system_prompt),
            &messages,
            0.2,
            None,
            configured_thinking,
            |delta| {
                emit_event(out_tx, req_id, "stream_chunk", json!({ "text": delta }));
            },
        )
        .await;
        return match res {
            Ok(full) => {
                persist_session_turn(&state, params.session_id.as_deref(), original_messages, &full).await;
                RpcResponse::ok(req.id, json!({ "text": full, "route": route }))
            }
            Err(e) => RpcResponse::err(req.id, e),
        };
    }
    if route.provider == ProviderKind::OpenAi && keys.has_openai() && !keys.has_gemini() {
        let system_prompt = format!(
            "You are Rursor Agent (text fallback mode — Gemini key not configured for tool calls). Provide exact, actionable code changes.{rules_block}{memory_block}"
        );
        let res = stream_openai_chat(
            &state.http,
            keys.openai.as_deref().unwrap_or_default(),
            &route.model,
            Some(&system_prompt),
            &messages,
            0.2,
            None,
            |delta| {
                emit_event(out_tx, req_id, "stream_chunk", json!({ "text": delta }));
            },
        )
        .await;
        return match res {
            Ok(full) => {
                persist_session_turn(&state, params.session_id.as_deref(), original_messages, &full).await;
                RpcResponse::ok(req.id, json!({ "text": full, "route": route }))
            }
            Err(e) => RpcResponse::err(req.id, e),
        };
    }

    let Some(api_key) = keys.gemini else {
        return RpcResponse::err(req.id, "Gemini API key is not configured. Run 'Rursor: Set Gemini API Key'.");
    };
    let model = if route.provider == ProviderKind::Gemini {
        route.model.clone()
    } else {
        default_model
    };

    let mut contents: Vec<Value> = messages.iter().map(chat_message_to_gemini_content).collect();
    let mcp_decls = state.mcp.read().await.gemini_function_declarations();
    let mode = normalize_agent_mode(&params.mode);
    let all_tools = agent_tool_declarations(&mcp_decls);
    let tools = Some(filter_tools_for_mode(mode, all_tools));
    let workspace_root_label = workspace_root
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "none".to_string());

    let system_prompt = match mode {
        "plan" => format!(
            "You are Rursor Plan mode inside a macOS code editor (workspace root: {workspace_root_label}).\n\
            Explore the codebase with read-only tools and produce a concrete implementation plan.\n\
            Rules:\n\
            - Do NOT edit files, write files, or run mutating commands.\n\
            - Use search/read/grep/outline/git tools and spawn_subagents for parallel research when helpful.\n\
            - Output a structured plan with: Goal, Assumptions, Steps (numbered), Files to touch, Risks, and a short Verification checklist.\n\
            - Prefer actionable steps a coding agent can execute later.{rules_block}{memory_block}"
        ),
        "take-control" => format!(
            "You are Rursor Computer-Use mode inside a macOS code editor (workspace root: {workspace_root_label}).\n\
            You can operate the user's computer through the workspace terminal and Rursor's real Chromium CDP browser tools.\n\
            Prefer this loop: browser_navigate → browser_snapshot → browser_act (and browser_record_walkthrough for proof), plus run_command for local automation.\n\
            Rules:\n\
            - Be explicit about each action you take and why.\n\
            - Prefer browser tools for UI/web tasks; use run_command for shell/system tasks in the workspace.\n\
            - Always read a file before editing it unless creating a brand new file.\n\
            - Prefer minimal, exact replacements in edit_file.\n\
            - Do not run full builds/tests unless needed for the computer-use task.\n\
            - Summarize actions taken and files changed.{rules_block}{memory_block}"
        ),
        _ => format!(
            "You are Rursor Agent, an autonomous AI software engineer inside a macOS code editor (workspace root: {workspace_root_label}).\n\
            Use the provided tools to inspect files, search/grep/symbol-search/AST-call-graph the workspace, check git diff, edit or create files, run parallel research subagents, manage background PTY processes, use git worktree sandboxes, inspect localhost web previews, persist learned project memories, invoke MCP tools, and run terminal commands.\n\
            Rules:\n\
            - Always read a file before editing it unless creating a brand new file.\n\
            - Prefer minimal, exact replacements in edit_file (`old_text` must uniquely match existing text in the file).\n\
            - Match effort to the request: for simple/local edits, change the files and stop. Do NOT run build, test, typecheck, lint, or install commands unless the user asked, the change is high-risk (shared APIs, build config, deps), or you hit a concrete compile/runtime error that needs diagnosis.\n\
            - Never run `npm run build`, `cargo build --release`, full test suites, or package installs as a default after-edit ritual.\n\
            - When verification is actually needed, prefer the lightest check (`tsc --noEmit`, `npm run compile`, targeted test) over a full production build.\n\
            - Summarize the exact files modified; only mention commands you actually ran.\n\
            - Spawn parallel subagents with spawn_subagents when research can be fan-out across roles.{rules_block}{memory_block}"
        ),
    };

    let enable_cache = *state.enable_prompt_cache.read().await;
    let static_prefix = format!("{rules_block}{memory_block}");
    let cached_content = if enable_cache && static_prefix.len() >= MIN_PROMPT_CACHE_CHARS {
        state
            .gemini
            .create_cached_content(&api_key, &model, Some(&system_prompt), &static_prefix, 600)
            .await
            .ok()
    } else {
        None
    };

    let auto_verify = params.auto_verify.unwrap_or(*state.auto_verify.read().await);
    let mut mutated_files = false;
    let mut checkpoint_created = false;
    let mut verify_attempts: u8 = 0;
    let mut combined_text = String::new();

    for step in 0..MAX_AGENT_STEPS {
        if step > 0 && !combined_text.is_empty() && !combined_text.ends_with('\n') {
            emit_event(out_tx, req_id, "stream_chunk", json!({ "text": "\n" }));
            combined_text.push('\n');
        }

        let turn_options = GeminiTurnOptions {
            thinking_budget: configured_thinking,
            cached_content: cached_content.clone(),
        };
        let sys_for_turn = if cached_content.is_some() {
            None
        } else {
            Some(system_prompt.as_str())
        };

        let turn = match state
            .gemini
            .stream_turn_with_options(
                &api_key,
                &model,
                sys_for_turn,
                &contents,
                tools.clone(),
                0.2,
                None,
                turn_options,
                |delta, kind| {
                    match kind {
                        gemini::GeminiStreamKind::Thought => {
                            emit_event(
                                out_tx,
                                req_id,
                                "thinking_chunk",
                                json!({ "text": delta }),
                            );
                        }
                        gemini::GeminiStreamKind::Text => {
                            emit_event(out_tx, req_id, "stream_chunk", json!({ "text": delta }));
                        }
                    }
                },
            )
            .await
        {
            Ok(t) => t,
            Err(e) => return RpcResponse::err(req.id, e),
        };

        combined_text.push_str(&turn.text);

        if turn.function_calls.is_empty() {
            if mutated_files && auto_verify && verify_attempts < 2 {
                if let Some(root) = workspace_root.as_ref() {
                    let cmds = detect_verify_commands(root);
                    if !cmds.is_empty() {
                        verify_attempts += 1;
                        let mut results = Vec::new();
                        for cmd in cmds {
                            let call_id = format!("verify_{}", NEXT_TOOL_CALL_ID.fetch_add(1, Ordering::Relaxed));
                            emit_event(
                                out_tx,
                                req_id,
                                "tool_event",
                                json!({
                                    "callId": call_id,
                                    "phase": "start",
                                    "name": format!("auto_verify: {}", cmd.label),
                                    "args": { "command": cmd.command },
                                }),
                            );
                            let (exit_code, output) = match run_pty_command(&cmd.command, root, 60, |_| {}).await {
                                Ok(res) => res,
                                Err(e) => (1, e),
                            };
                            emit_event(
                                out_tx,
                                req_id,
                                "tool_event",
                                json!({
                                    "callId": call_id,
                                    "phase": "done",
                                    "name": format!("auto_verify: {}", cmd.label),
                                    "ok": exit_code == 0,
                                    "output": { "exitCode": exit_code },
                                }),
                            );
                            results.push(VerifyResult {
                                label: cmd.label,
                                command: cmd.command,
                                exit_code,
                                output,
                            });
                        }
                        if !all_verify_passed(&results) {
                            let feedback = format_verify_feedback(&results);
                            contents.push(json!({
                                "role": "model",
                                "parts": coalesce_model_parts(turn.raw_model_parts)
                            }));
                            contents.push(json!({
                                "role": "user",
                                "parts": [{ "text": feedback }]
                            }));
                            continue;
                        }
                    }
                }
            }

            persist_session_turn(
                &state,
                params.session_id.as_deref(),
                original_messages,
                &combined_text,
            )
            .await;
            return RpcResponse::ok(req.id, json!({ "text": combined_text, "route": route }));
        }

        contents.push(json!({
            "role": "model",
            "parts": coalesce_model_parts(turn.raw_model_parts)
        }));

        let mut response_parts = Vec::new();
        for fc in turn.function_calls {
            if (fc.name == "edit_file" || fc.name == "write_file") && !checkpoint_created {
                checkpoint_created = true;
                if let Some(root) = workspace_root.as_ref() {
                    if let Ok(summary) =
                        state
                            .checkpoints
                            .create_checkpoint(root, "Pre-edit agent checkpoint", &[])
                    {
                        emit_event(
                            out_tx,
                            req_id,
                            "checkpoint_created",
                            json!({ "checkpoint": summary }),
                        );
                    }
                }
            }
            if fc.name == "edit_file" || fc.name == "write_file" {
                mutated_files = true;
            }

            let call_id = format!("tc_{}", NEXT_TOOL_CALL_ID.fetch_add(1, Ordering::Relaxed));
            let result_val = execute_agent_tool(
                req_id,
                &call_id,
                &fc.name,
                &fc.args,
                &state,
                out_tx,
                &api_key,
                &model,
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

    let suffix = "\n\n[Stopped after reaching max agent steps (16).]";
    emit_event(out_tx, req_id, "stream_chunk", json!({ "text": suffix }));
    combined_text.push_str(suffix);
    persist_session_turn(
        &state,
        params.session_id.as_deref(),
        original_messages,
        &combined_text,
    )
    .await;
    RpcResponse::ok(req.id, json!({ "text": combined_text, "route": route }))
}

async fn persist_session_turn(
    state: &AppState,
    session_id: Option<&str>,
    mut messages: Vec<ChatMessage>,
    assistant_text: &str,
) {
    let Some(id) = session_id else {
        return;
    };
    messages.push(ChatMessage {
        role: "assistant".to_string(),
        content: assistant_text.to_string(),
        images: vec![],
    });
    let title = messages
        .iter()
        .find(|m| m.role == "user")
        .map(|m| m.content.lines().next().unwrap_or("Session").chars().take(48).collect::<String>())
        .unwrap_or_else(|| "Session".to_string());
    let root = state.index.read().await.root.clone();
    let _ = state.sessions.save(root.as_deref(), id, &title, messages);
}

#[allow(clippy::too_many_arguments)]
async fn execute_agent_tool(
    req_id: u64,
    call_id: &str,
    name: &str,
    args: &Value,
    state: &Arc<AppState>,
    out_tx: &mpsc::UnboundedSender<String>,
    api_key: &str,
    model: &str,
) -> Value {
    match name {
        "read_file"
        | "list_files"
        | "search_workspace"
        | "search_symbols"
        | "find_callers"
        | "file_outline"
        | "grep_workspace"
        | "git_context" => {
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
                    "search_symbols" => {
                        let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
                        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(25) as usize;
                        let hits = index.search_symbols(query, limit);
                        (true, json!({ "symbols": hits }))
                    }
                    "find_callers" => {
                        let symbol = args.get("symbol").and_then(|v| v.as_str()).unwrap_or("");
                        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
                        let callers = index.find_callers(symbol, limit);
                        (true, json!({ "callers": callers }))
                    }
                    "file_outline" => {
                        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
                        let chunks = index.file_outline(path);
                        (true, json!({ "chunks": chunks }))
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
                    "git_context" => match index.git_context() {
                        Ok(ctx) => (true, json!({ "context": ctx })),
                        Err(e) => (false, json!({ "error": e })),
                    },
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
        "save_memory" => {
            let root = state.index.read().await.root.clone();
            let scope = args.get("scope").and_then(|v| v.as_str()).unwrap_or("workspace");
            let category = args.get("category").and_then(|v| v.as_str()).unwrap_or("convention");
            let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
            match save_memory(root.as_deref(), scope, category, content) {
                Ok(entry) => json!({ "ok": true, "result": { "memory": entry } }),
                Err(e) => json!({ "ok": false, "result": { "error": e } }),
            }
        }
        "spawn_subagents" => {
            emit_event(
                out_tx,
                req_id,
                "tool_event",
                json!({ "callId": call_id, "phase": "start", "name": "spawn_subagents", "args": args }),
            );
            let tasks_val = args.get("tasks").cloned().unwrap_or_else(|| json!([]));
            let tasks: Vec<SubagentTask> = match serde_json::from_value(tasks_val) {
                Ok(t) => t,
                Err(e) => return json!({ "ok": false, "result": { "error": format!("Invalid subagent tasks: {e}") } }),
            };
            let rules = state.workspace_rules.read().await.clone();
            let results = run_parallel_subagents(
                &state.gemini,
                api_key,
                model,
                rules.as_deref(),
                tasks,
                |task_id, role, status, output| {
                    emit_event(
                        out_tx,
                        req_id,
                        "subagent_event",
                        json!({
                            "taskId": task_id,
                            "role": role,
                            "status": status,
                            "output": output,
                        }),
                    );
                },
            )
            .await;
            emit_event(
                out_tx,
                req_id,
                "tool_event",
                json!({ "callId": call_id, "phase": "done", "name": "spawn_subagents", "ok": true, "output": { "count": results.len() } }),
            );
            json!({ "ok": true, "result": { "results": results } })
        }
        "start_background_process" => {
            let command = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
            let Some(root) = state.index.read().await.root.clone() else {
                return json!({ "ok": false, "result": { "error": "No workspace root open." } });
            };
            match state.pty_manager.start_background(command, &root) {
                Ok(info) => json!({ "ok": true, "result": { "process": info } }),
                Err(e) => json!({ "ok": false, "result": { "error": e } }),
            }
        }
        "read_background_process" => {
            let proc_id = args.get("proc_id").and_then(|v| v.as_str()).unwrap_or("");
            match state.pty_manager.read_output(proc_id) {
                Ok(out) => json!({ "ok": true, "result": { "process": out } }),
                Err(e) => json!({ "ok": false, "result": { "error": e } }),
            }
        }
        "stop_background_process" => {
            let proc_id = args.get("proc_id").and_then(|v| v.as_str()).unwrap_or("");
            match state.pty_manager.stop_process(proc_id) {
                Ok(stopped) => json!({ "ok": true, "result": { "stopped": stopped } }),
                Err(e) => json!({ "ok": false, "result": { "error": e } }),
            }
        }
        "create_worktree" => {
            let task_name = args.get("task_name").and_then(|v| v.as_str()).unwrap_or("task");
            let Some(root) = state.index.read().await.root.clone() else {
                return json!({ "ok": false, "result": { "error": "No workspace root open." } });
            };
            match create_worktree(&root, task_name) {
                Ok(info) => json!({ "ok": true, "result": { "worktree": info } }),
                Err(e) => json!({ "ok": false, "result": { "error": e } }),
            }
        }
        "diff_worktree" => {
            let wt_path = args.get("worktree_path").and_then(|v| v.as_str()).unwrap_or("");
            match diff_worktree(&PathBuf::from(wt_path)) {
                Ok(diff) => json!({ "ok": true, "result": { "diff": diff } }),
                Err(e) => json!({ "ok": false, "result": { "error": e } }),
            }
        }
        "repo_map" => {
            let budget = args
                .get("token_budget_chars")
                .and_then(|v| v.as_u64())
                .unwrap_or(6_000) as usize;
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            let files: Vec<RepoFileInput> = {
                let index = state.index.read().await;
                index
                    .files
                    .iter()
                    .take(600)
                    .map(|f| RepoFileInput {
                        path: f.rel_path.clone(),
                        content: f.content.clone(),
                    })
                    .collect()
            };
            let query_terms: Vec<String> = query
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .filter(|w| w.len() >= 3)
                .map(str::to_string)
                .collect();
            let seed = RepoMapSeed {
                active_files: Vec::new(),
                dirty_files: Vec::new(),
                query_terms,
            };
            let map = build_pagerank_repo_map(&files, &seed, budget);
            json!({ "ok": true, "result": { "repoMap": map } })
        }
        "create_cow_clone" => {
            let label = args.get("task_label").and_then(|v| v.as_str()).unwrap_or("agent-sandbox");
            let Some(root) = state.index.read().await.root.clone() else {
                return json!({ "ok": false, "result": { "error": "No workspace root open." } });
            };
            match state.cow_clones.create_clone(&root, label) {
                Ok(meta) => json!({ "ok": true, "result": { "clone": meta } }),
                Err(e) => json!({ "ok": false, "result": { "error": e } }),
            }
        }
        "diff_cow_clone" => {
            let clone_id = args.get("clone_id").and_then(|v| v.as_str()).unwrap_or("");
            match state.cow_clones.diff_clone(clone_id) {
                Ok(diffs) => json!({ "ok": true, "result": { "diffs": diffs } }),
                Err(e) => json!({ "ok": false, "result": { "error": e } }),
            }
        }
        "apply_cow_clone" => {
            let clone_id = args.get("clone_id").and_then(|v| v.as_str()).unwrap_or("");
            match state.cow_clones.apply_clone_back(clone_id, None) {
                Ok(applied) => json!({ "ok": true, "result": { "appliedFiles": applied } }),
                Err(e) => json!({ "ok": false, "result": { "error": e } }),
            }
        }
        "destroy_cow_clone" => {
            let clone_id = args.get("clone_id").and_then(|v| v.as_str()).unwrap_or("");
            match state.cow_clones.destroy_clone(clone_id) {
                Ok(destroyed) => json!({ "ok": true, "result": { "destroyed": destroyed } }),
                Err(e) => json!({ "ok": false, "result": { "error": e } }),
            }
        }
        "spawn_worker_swarm" => {
            emit_event(
                out_tx,
                req_id,
                "tool_event",
                json!({ "callId": call_id, "phase": "start", "name": name, "args": args }),
            );
            let goal = args.get("goal").and_then(|v| v.as_str()).unwrap_or("");
            let max_workers = args
                .get("max_parallel_workers")
                .and_then(|v| v.as_u64())
                .unwrap_or(3) as usize;
            let Some(root) = state.index.read().await.root.clone() else {
                return json!({ "ok": false, "result": { "error": "No workspace root open." } });
            };
            let router = state.smart_router().await;
            let keys = state.provider_keys().await;
            let allow_net = *state.sandbox_allow_network.read().await;
            let tx = out_tx.clone();
            let (ok, output) = match run_swarm_mission(
                goal,
                None,
                "",
                max_workers,
                6,
                allow_net,
                Some(model),
                &root,
                &state.cow_clones,
                &router,
                &keys,
                Arc::new(move |ev| {
                    if let Ok(val) = serde_json::to_value(ev) {
                        emit_event(&tx, req_id, "swarm_event", val);
                    }
                }),
            )
            .await
            {
                Ok(rep) => (true, serde_json::to_value(rep).unwrap_or_else(|_| json!({}))),
                Err(e) => (false, json!({ "error": e })),
            };
            emit_event(
                out_tx,
                req_id,
                "tool_event",
                json!({ "callId": call_id, "phase": "done", "name": name, "ok": ok, "output": output }),
            );
            json!({ "ok": ok, "result": output })
        }
        "browser_navigate" | "browser_snapshot" | "browser_act" | "browser_record_walkthrough" => {
            emit_event(
                out_tx,
                req_id,
                "tool_event",
                json!({ "callId": call_id, "phase": "start", "name": name, "args": args }),
            );
            let (ok, output) = match state.browser.handle_rpc(name, args.clone()).await {
                Ok(val) => {
                    if name == "browser_record_walkthrough" {
                        if let Some(mp4_path) = val.get("outputMp4Path").and_then(|v| v.as_str()) {
                            emit_event(
                                out_tx,
                                req_id,
                                "artifact_created",
                                json!({
                                    "kind": "video",
                                    "title": "Browser Walkthrough Proof",
                                    "path": mp4_path,
                                }),
                            );
                        }
                    }
                    (true, val)
                }
                Err(e) => (false, json!({ "error": e })),
            };
            emit_event(
                out_tx,
                req_id,
                "tool_event",
                json!({ "callId": call_id, "phase": "done", "name": name, "ok": ok, "output": output }),
            );
            json!({ "ok": ok, "result": output })
        }
        "create_presentation" => {
            emit_event(
                out_tx,
                req_id,
                "tool_event",
                json!({ "callId": call_id, "phase": "start", "name": name, "args": args }),
            );
            let Some(root) = state.index.read().await.root.clone() else {
                return json!({ "ok": false, "result": { "error": "No workspace root open." } });
            };
            let (ok, output) = match serde_json::from_value::<PresentationSpec>(args.clone()) {
                Ok(spec) => match create_presentation(&root, &spec) {
                    Ok(art) => {
                        emit_event(
                            out_tx,
                            req_id,
                            "artifact_created",
                            json!({
                                "kind": "presentation",
                                "title": spec.title,
                                "pptxPath": art.pptx_path,
                                "htmlDeckPath": art.html_deck_path,
                                "slideCount": art.slide_count,
                            }),
                        );
                        (true, serde_json::to_value(art).unwrap_or_else(|_| json!({})))
                    }
                    Err(e) => (false, json!({ "error": e })),
                },
                Err(e) => (false, json!({ "error": format!("Invalid presentation spec: {e}") })),
            };
            emit_event(
                out_tx,
                req_id,
                "tool_event",
                json!({ "callId": call_id, "phase": "done", "name": name, "ok": ok, "output": output }),
            );
            json!({ "ok": ok, "result": output })
        }
        "create_explainer_video" => {
            emit_event(
                out_tx,
                req_id,
                "tool_event",
                json!({ "callId": call_id, "phase": "start", "name": name, "args": args }),
            );
            let Some(root) = state.index.read().await.root.clone() else {
                return json!({ "ok": false, "result": { "error": "No workspace root open." } });
            };
            let (ok, output) = match serde_json::from_value::<VideoStoryboardSpec>(args.clone()) {
                Ok(spec) => {
                    let video_title = spec.title.clone();
                    match render_explainer_video(&root, &spec).await {
                        Ok(art) => {
                            emit_event(
                                out_tx,
                                req_id,
                                "artifact_created",
                                json!({
                                    "kind": "video",
                                    "title": video_title,
                                    "path": art.video_path,
                                    "durationSecs": art.duration_secs,
                                }),
                            );
                            (true, serde_json::to_value(art).unwrap_or_else(|_| json!({})))
                        }
                        Err(e) => (false, json!({ "error": e })),
                    }
                }
                Err(e) => (false, json!({ "error": format!("Invalid video storyboard spec: {e}") })),
            };
            emit_event(
                out_tx,
                req_id,
                "tool_event",
                json!({ "callId": call_id, "phase": "done", "name": name, "ok": ok, "output": output }),
            );
            json!({ "ok": ok, "result": output })
        }
        "generate_image" => {
            emit_event(
                out_tx,
                req_id,
                "tool_event",
                json!({ "callId": call_id, "phase": "start", "name": name, "args": args }),
            );
            let Some(root) = state.index.read().await.root.clone() else {
                return json!({ "ok": false, "result": { "error": "No workspace root open." } });
            };
            let (ok, output) = match serde_json::from_value::<ImageAssetSpec>(args.clone()) {
                Ok(spec) => {
                    let key_opt = if api_key.trim().is_empty() { None } else { Some(api_key) };
                    match generate_image_asset(&state.http, key_opt, &root, &spec).await {
                        Ok(art) => {
                            emit_event(
                                out_tx,
                                req_id,
                                "artifact_created",
                                json!({
                                    "kind": "image",
                                    "title": spec.prompt,
                                    "path": art.image_path,
                                }),
                            );
                            (true, serde_json::to_value(art).unwrap_or_else(|_| json!({})))
                        }
                        Err(e) => (false, json!({ "error": e })),
                    }
                }
                Err(e) => (false, json!({ "error": format!("Invalid image asset spec: {e}") })),
            };
            emit_event(
                out_tx,
                req_id,
                "tool_event",
                json!({ "callId": call_id, "phase": "done", "name": name, "ok": ok, "output": output }),
            );
            json!({ "ok": ok, "result": output })
        }
        "create_spreadsheet" => {
            emit_event(
                out_tx,
                req_id,
                "tool_event",
                json!({ "callId": call_id, "phase": "start", "name": name, "args": args }),
            );
            let Some(root) = state.index.read().await.root.clone() else {
                return json!({ "ok": false, "result": { "error": "No workspace root open." } });
            };
            let (ok, output) = match serde_json::from_value::<SpreadsheetSpec>(args.clone()) {
                Ok(spec) => match create_spreadsheet(&root, &spec) {
                    Ok(art) => {
                        emit_event(
                            out_tx,
                            req_id,
                            "artifact_created",
                            json!({
                                "kind": "spreadsheet",
                                "title": spec.output_path,
                                "path": art.xlsx_path,
                            }),
                        );
                        (true, serde_json::to_value(art).unwrap_or_else(|_| json!({})))
                    }
                    Err(e) => (false, json!({ "error": e })),
                },
                Err(e) => (false, json!({ "error": format!("Invalid spreadsheet spec: {e}") })),
            };
            emit_event(
                out_tx,
                req_id,
                "tool_event",
                json!({ "callId": call_id, "phase": "done", "name": name, "ok": ok, "output": output }),
            );
            json!({ "ok": ok, "result": output })
        }
        "create_document" => {
            emit_event(
                out_tx,
                req_id,
                "tool_event",
                json!({ "callId": call_id, "phase": "start", "name": name, "args": args }),
            );
            let Some(root) = state.index.read().await.root.clone() else {
                return json!({ "ok": false, "result": { "error": "No workspace root open." } });
            };
            let (ok, output) = match serde_json::from_value::<DocumentSpec>(args.clone()) {
                Ok(spec) => {
                    let doc_title = spec.title.clone();
                    match create_document(&root, &spec) {
                        Ok(art) => {
                            emit_event(
                                out_tx,
                                req_id,
                                "artifact_created",
                                json!({
                                    "kind": "document",
                                    "title": doc_title,
                                    "path": art.docx_path,
                                }),
                            );
                            (true, serde_json::to_value(art).unwrap_or_else(|_| json!({})))
                        }
                        Err(e) => (false, json!({ "error": e })),
                    }
                }
                Err(e) => (false, json!({ "error": format!("Invalid document spec: {e}") })),
            };
            emit_event(
                out_tx,
                req_id,
                "tool_event",
                json!({ "callId": call_id, "phase": "done", "name": name, "ok": ok, "output": output }),
            );
            json!({ "ok": ok, "result": output })
        }
        "render_ui_widget" => {
            let title = args
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("Interactive Widget");
            let html = args.get("html").and_then(|v| v.as_str()).unwrap_or("");
            emit_event(
                out_tx,
                req_id,
                "artifact_created",
                json!({
                    "kind": "gen_ui",
                    "title": title,
                    "html": html,
                }),
            );
            json!({ "ok": true, "result": { "rendered": true, "title": title } })
        }
        "web_search" => {
            emit_event(
                out_tx,
                req_id,
                "tool_event",
                json!({ "callId": call_id, "phase": "start", "name": name, "args": args }),
            );
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(6) as usize;
            let key_opt = if api_key.trim().is_empty() { None } else { Some(api_key) };
            let (ok, output) = match web_search(&state.http, query, limit, key_opt, Some(model)).await {
                Ok(res) => (true, serde_json::to_value(res).unwrap_or_else(|_| json!({}))),
                Err(e) => (false, json!({ "error": e })),
            };
            emit_event(
                out_tx,
                req_id,
                "tool_event",
                json!({ "callId": call_id, "phase": "done", "name": name, "ok": ok, "output": output }),
            );
            json!({ "ok": ok, "result": output })
        }
        "read_url_markdown" => {
            emit_event(
                out_tx,
                req_id,
                "tool_event",
                json!({ "callId": call_id, "phase": "start", "name": name, "args": args }),
            );
            let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let (ok, output) = match fetch_url_as_markdown(&state.http, url, 24_000, false).await {
                Ok(res) => (true, serde_json::to_value(res).unwrap_or_else(|_| json!({}))),
                Err(e) => (false, json!({ "error": e })),
            };
            emit_event(
                out_tx,
                req_id,
                "tool_event",
                json!({ "callId": call_id, "phase": "done", "name": name, "ok": ok, "output": output }),
            );
            json!({ "ok": ok, "result": output })
        }
        "search_docs" => {
            emit_event(
                out_tx,
                req_id,
                "tool_event",
                json!({ "callId": call_id, "phase": "start", "name": name, "args": args }),
            );
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            let source = args.get("source").and_then(|v| v.as_str());
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(6) as usize;
            let (ok, output) = match state.docs_index.search_docs(query, source, limit) {
                Ok(hits) => (true, json!({ "hits": hits })),
                Err(e) => (false, json!({ "error": e })),
            };
            emit_event(
                out_tx,
                req_id,
                "tool_event",
                json!({ "callId": call_id, "phase": "done", "name": name, "ok": ok, "output": output }),
            );
            json!({ "ok": ok, "result": output })
        }
        "edit_file" | "write_file" | "run_command" | "inspect_web_page" => {
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
        other => {
            if state.mcp.read().await.has_tool(other) {
                emit_event(
                    out_tx,
                    req_id,
                    "tool_event",
                    json!({
                        "callId": call_id,
                        "phase": "start",
                        "name": other,
                        "args": args,
                    }),
                );
                let (ok, output) = match state.mcp.read().await.call_tool(other, args.clone()).await {
                    Ok(res) => (true, res),
                    Err(e) => (false, json!({ "error": e })),
                };
                emit_event(
                    out_tx,
                    req_id,
                    "tool_event",
                    json!({
                        "callId": call_id,
                        "phase": "done",
                        "name": other,
                        "ok": ok,
                        "output": output,
                    }),
                );
                json!({ "ok": ok, "result": output })
            } else {
                json!({
                    "ok": false,
                    "result": { "error": format!("Unknown tool: {other}") }
                })
            }
        }
    }
}

fn normalize_agent_mode(mode: &str) -> &'static str {
    match mode.trim().to_ascii_lowercase().as_str() {
        "ask" => "ask",
        "plan" => "plan",
        "take-control" | "take_control" | "computer" | "computer-use" | "pc" => "take-control",
        _ => "agent",
    }
}

fn filter_tools_for_mode(mode: &str, mut tools: Vec<Value>) -> Vec<Value> {
    const PLAN_ALLOW: &[&str] = &[
        "read_file",
        "list_files",
        "search_workspace",
        "search_symbols",
        "find_callers",
        "file_outline",
        "grep_workspace",
        "git_context",
        "repo_map",
        "web_search",
        "spawn_subagents",
        "inspect_web_page",
        "browser_snapshot",
        "search_docs",
    ];
    match mode {
        "plan" => {
            tools.retain(|t| {
                t.get("name")
                    .and_then(|n| n.as_str())
                    .map(|name| PLAN_ALLOW.contains(&name) || name.starts_with("mcp_"))
                    .unwrap_or(false)
            });
            tools
        }
        _ => tools,
    }
}

fn agent_tool_declarations(mcp_decls: &[Value]) -> Vec<Value> {
    let mut decls = vec![
        json!({
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
        }),
        json!({
            "name": "list_files",
            "description": "List indexed files in the workspace, optionally filtered by directory prefix.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "prefix": { "type": "STRING", "description": "Optional relative directory prefix." },
                    "limit": { "type": "INTEGER", "description": "Max files to return (default 200)." }
                }
            }
        }),
        json!({
            "name": "search_workspace",
            "description": "Hybrid BM25 + semantic AST search across indexed workspace files.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "query": { "type": "STRING", "description": "Space-separated keywords or symbol names." },
                    "limit": { "type": "INTEGER", "description": "Max hits to return (default 8)." }
                },
                "required": ["query"]
            }
        }),
        json!({
            "name": "search_symbols",
            "description": "Search extracted Tree-sitter AST symbols (functions, structs, classes, interfaces, types) across the workspace.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "query": { "type": "STRING", "description": "Symbol substring to match." },
                    "limit": { "type": "INTEGER", "description": "Max symbols to return (default 25)." }
                },
                "required": ["query"]
            }
        }),
        json!({
            "name": "find_callers",
            "description": "Find AST call-graph references/callers of a symbol across the workspace.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "symbol": { "type": "STRING", "description": "Exact symbol or function name." },
                    "limit": { "type": "INTEGER", "description": "Max caller sites (default 20)." }
                },
                "required": ["symbol"]
            }
        }),
        json!({
            "name": "file_outline",
            "description": "Get the Tree-sitter AST outline (functions, classes, signatures, calls) for a workspace file.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "path": { "type": "STRING", "description": "Workspace-relative file path." }
                },
                "required": ["path"]
            }
        }),
        json!({
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
        }),
        json!({
            "name": "git_context",
            "description": "Inspect git status and current unstaged git diff in the workspace.",
            "parameters": {
                "type": "OBJECT",
                "properties": {}
            }
        }),
        json!({
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
        }),
        json!({
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
        }),
        json!({
            "name": "run_command",
            "description": "Run a shell command in the workspace root only when needed (debug a failure, user asked, or high-risk change). Do not use for routine after-edit builds like `npm run build`. Prefer light checks such as `npm run compile` or `npx tsc --noEmit` when verification is required. Returns stdout/stderr/exit code.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "command": { "type": "STRING", "description": "Shell command to execute in workspace root." },
                    "reason": { "type": "STRING", "description": "Why this command is necessary right now (not habit)." }
                },
                "required": ["command"]
            }
        }),
        json!({
            "name": "save_memory",
            "description": "Persist a durable project or user convention into Rursor's cross-session memory.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "scope": { "type": "STRING", "description": "'workspace' or 'global'." },
                    "category": { "type": "STRING", "description": "Short category like 'architecture', 'testing', or 'style'." },
                    "content": { "type": "STRING", "description": "Concise fact or convention to remember." }
                },
                "required": ["content"]
            }
        }),
        json!({
            "name": "spawn_subagents",
            "description": "Run multiple parallel read-only research/analysis subtasks concurrently.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "tasks": {
                        "type": "ARRAY",
                        "description": "List of { id, role, prompt } subtask objects.",
                        "items": {
                            "type": "OBJECT",
                            "properties": {
                                "id": { "type": "STRING" },
                                "role": { "type": "STRING" },
                                "prompt": { "type": "STRING" }
                            }
                        }
                    }
                },
                "required": ["tasks"]
            }
        }),
        json!({
            "name": "start_background_process",
            "description": "Start a dev server or long-running process inside a background PTY.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "command": { "type": "STRING", "description": "Shell command to run in background PTY." }
                },
                "required": ["command"]
            }
        }),
        json!({
            "name": "read_background_process",
            "description": "Read unread PTY output and status from a background process.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "proc_id": { "type": "STRING", "description": "Process ID returned by start_background_process." }
                },
                "required": ["proc_id"]
            }
        }),
        json!({
            "name": "stop_background_process",
            "description": "Stop a running background PTY process.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "proc_id": { "type": "STRING", "description": "Process ID returned by start_background_process." }
                },
                "required": ["proc_id"]
            }
        }),
        json!({
            "name": "create_worktree",
            "description": "Create an isolated git worktree sandbox for parallel or risky task changes.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "task_name": { "type": "STRING", "description": "Short task slug for the worktree branch." }
                },
                "required": ["task_name"]
            }
        }),
        json!({
            "name": "diff_worktree",
            "description": "Inspect git diff inside a worktree sandbox.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "worktree_path": { "type": "STRING", "description": "Filesystem path of the worktree." }
                },
                "required": ["worktree_path"]
            }
        }),
        json!({
            "name": "inspect_web_page",
            "description": "Open a URL in Rursor's built-in Web Preview (optional) and inspect HTTP status, title, console/script errors, and visible text.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "url": { "type": "STRING", "description": "HTTP or HTTPS URL (e.g. http://localhost:3000)." },
                    "open_preview": { "type": "BOOLEAN", "description": "Whether to open/focus the Web Preview panel." }
                },
                "required": ["url"]
            }
        }),
        json!({
            "name": "repo_map",
            "description": "Generate a Personalized PageRank structural repository map of top-ranked files and signatures.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "query": { "type": "STRING", "description": "Optional query keywords to seed Personalized PageRank." },
                    "token_budget_chars": { "type": "INTEGER", "description": "Character budget for the returned map (default 6000)." }
                }
            }
        }),
        json!({
            "name": "create_cow_clone",
            "description": "Create an instant zero-overhead macOS APFS Copy-on-Write workspace sandbox clone.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "task_label": { "type": "STRING", "description": "Short task slug for the CoW clone." }
                },
                "required": ["task_label"]
            }
        }),
        json!({
            "name": "diff_cow_clone",
            "description": "Inspect file-level diffs inside an APFS Copy-on-Write workspace clone.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "clone_id": { "type": "STRING", "description": "Clone ID returned by create_cow_clone." }
                },
                "required": ["clone_id"]
            }
        }),
        json!({
            "name": "apply_cow_clone",
            "description": "Apply changes from an APFS Copy-on-Write clone back to the main workspace.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "clone_id": { "type": "STRING", "description": "Clone ID returned by create_cow_clone." }
                },
                "required": ["clone_id"]
            }
        }),
        json!({
            "name": "destroy_cow_clone",
            "description": "Delete an APFS Copy-on-Write workspace clone.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "clone_id": { "type": "STRING", "description": "Clone ID returned by create_cow_clone." }
                },
                "required": ["clone_id"]
            }
        }),
        json!({
            "name": "spawn_worker_swarm",
            "description": "Decompose a complex multi-part goal into a topological DAG of autonomous tool-using worker agents running in parallel APFS Copy-on-Write workspace clones.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "goal": { "type": "STRING", "description": "High-level engineering goal to decompose and execute in parallel." },
                    "max_parallel_workers": { "type": "INTEGER", "description": "Max concurrent worker agents (default 3)." }
                },
                "required": ["goal"]
            }
        }),
        json!({
            "name": "browser_navigate",
            "description": "Navigate Rursor's real Chromium CDP browser to an HTTP/HTTPS URL (including localhost dev servers).",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "url": { "type": "STRING", "description": "Target http:// or https:// URL." },
                    "viewportWidth": { "type": "INTEGER", "description": "Optional viewport width (default 1280)." },
                    "viewportHeight": { "type": "INTEGER", "description": "Optional viewport height (default 800)." }
                },
                "required": ["url"]
            }
        }),
        json!({
            "name": "browser_snapshot",
            "description": "Capture a Set-of-Mark [1..N] interactive element list, page text summary, and screenshot from Rursor's Chromium CDP browser.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "drawSomBadges": { "type": "BOOLEAN", "description": "Draw numbered [1][2] visual badges on interactive elements." },
                    "includeScreenshot": { "type": "BOOLEAN", "description": "Capture base64 PNG screenshot." }
                }
            }
        }),
        json!({
            "name": "browser_act",
            "description": "Interact with Rursor's Chromium CDP browser (`click`, `type`, `scroll`, `evaluate`, `clear_overlay`).",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "action": {
                        "type": "OBJECT",
                        "description": "Action object with `kind` ('click' | 'type' | 'scroll' | 'evaluate' | 'clear_overlay'), optional `somId`, `selector`, `text`, `pressEnter`."
                    }
                },
                "required": ["action"]
            }
        }),
        json!({
            "name": "browser_record_walkthrough",
            "description": "Execute a sequence of browser steps and encode an .mp4 video walkthrough proof.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "outputMp4Path": { "type": "STRING", "description": "Output .mp4 path (e.g. .rursor/artifacts/demo.mp4)." },
                    "steps": {
                        "type": "ARRAY",
                        "description": "Array of walkthrough step objects (navigate, click, type, scroll, wait_ms).",
                        "items": {
                            "type": "OBJECT",
                            "properties": {
                                "kind": { "type": "STRING" },
                                "url": { "type": "STRING" },
                                "somId": { "type": "INTEGER" },
                                "selector": { "type": "STRING" },
                                "text": { "type": "STRING" },
                                "waitMs": { "type": "INTEGER" }
                            }
                        }
                    }
                },
                "required": ["steps"]
            }
        }),
        json!({
            "name": "create_presentation",
            "description": "Generate both a native editable Microsoft PowerPoint (.pptx) deck and an interactive HTML5 slide deck (.deck.html).",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "title": { "type": "STRING", "description": "Presentation title." },
                    "subtitle": { "type": "STRING", "description": "Presentation subtitle." },
                    "author": { "type": "STRING", "description": "Author or team name." },
                    "theme": { "type": "STRING", "description": "'obsidian', 'executive', or 'minimal'." },
                    "outputPath": { "type": "STRING", "description": "Workspace-relative .pptx path (e.g. decks/roadmap.pptx)." },
                    "slides": {
                        "type": "ARRAY",
                        "description": "Array of slide objects ({ layout, title, subtitle, bullets, rightBullets, metrics, code, speakerNotes }).",
                        "items": {
                            "type": "OBJECT",
                            "properties": {
                                "layout": { "type": "STRING" },
                                "title": { "type": "STRING" },
                                "subtitle": { "type": "STRING" },
                                "bullets": { "type": "ARRAY", "items": { "type": "STRING" } },
                                "rightBullets": { "type": "ARRAY", "items": { "type": "STRING" } },
                                "code": { "type": "STRING" },
                                "speakerNotes": { "type": "STRING" }
                            }
                        }
                    }
                },
                "required": ["title", "outputPath", "slides"]
            }
        }),
        json!({
            "name": "create_explainer_video",
            "description": "Render an H.264 .mp4 explainer/product video on macOS with scene cards, code snippets, and optional synthesized voiceover narration.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "title": { "type": "STRING", "description": "Video title." },
                    "outputPath": { "type": "STRING", "description": "Workspace-relative .mp4 path (e.g. videos/overview.mp4)." },
                    "scenes": {
                        "type": "ARRAY",
                        "description": "Array of scenes ({ heading, subheading, bullets, codeSnippet, narration, durationSecs, accentHex }).",
                        "items": {
                            "type": "OBJECT",
                            "properties": {
                                "heading": { "type": "STRING" },
                                "subheading": { "type": "STRING" },
                                "bullets": { "type": "ARRAY", "items": { "type": "STRING" } },
                                "codeSnippet": { "type": "STRING" },
                                "narration": { "type": "STRING" },
                                "durationSecs": { "type": "NUMBER" },
                                "accentHex": { "type": "STRING" }
                            }
                        }
                    }
                },
                "required": ["title", "outputPath", "scenes"]
            }
        }),
        json!({
            "name": "generate_image",
            "description": "Generate a PNG/SVG illustration, UI mockup, or diagram card into the workspace.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "prompt": { "type": "STRING", "description": "Visual prompt or diagram heading." },
                    "outputPath": { "type": "STRING", "description": "Workspace-relative .png path (e.g. assets/hero.png)." },
                    "subtitle": { "type": "STRING", "description": "Optional subtitle for local card fallback." },
                    "bullets": {
                        "type": "ARRAY",
                        "description": "Optional key points for architecture/card rendering.",
                        "items": { "type": "STRING" }
                    }
                },
                "required": ["prompt", "outputPath"]
            }
        }),
        json!({
            "name": "create_spreadsheet",
            "description": "Create a native Excel (.xlsx) workbook with multiple sheets, numbers, and live Excel formulas (=SUM, =AVERAGE).",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "outputPath": { "type": "STRING", "description": "Workspace-relative .xlsx path (e.g. reports/metrics.xlsx)." },
                    "sheets": {
                        "type": "ARRAY",
                        "description": "Array of { name, headers, rows } sheet objects.",
                        "items": {
                            "type": "OBJECT",
                            "properties": {
                                "name": { "type": "STRING" },
                                "headers": { "type": "ARRAY", "items": { "type": "STRING" } },
                                "rows": {
                                    "type": "ARRAY",
                                    "items": { "type": "ARRAY", "items": { "type": "STRING" } }
                                }
                            }
                        }
                    }
                },
                "required": ["outputPath", "sheets"]
            }
        }),
        json!({
            "name": "create_document",
            "description": "Create a native Microsoft Word (.docx) document with headings, paragraphs, bullets, code blocks, and tables.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "title": { "type": "STRING", "description": "Document title." },
                    "subtitle": { "type": "STRING", "description": "Document subtitle." },
                    "outputPath": { "type": "STRING", "description": "Workspace-relative .docx path (e.g. docs/rfc.docx)." },
                    "sections": {
                        "type": "ARRAY",
                        "description": "Array of { heading, paragraphs, bullets, codeBlock, table } sections.",
                        "items": {
                            "type": "OBJECT",
                            "properties": {
                                "heading": { "type": "STRING" },
                                "paragraphs": { "type": "ARRAY", "items": { "type": "STRING" } },
                                "bullets": { "type": "ARRAY", "items": { "type": "STRING" } },
                                "codeBlock": { "type": "STRING" }
                            }
                        }
                    }
                },
                "required": ["title", "outputPath", "sections"]
            }
        }),
        json!({
            "name": "render_ui_widget",
            "description": "Render an interactive sandboxed HTML/SVG/Chart/Canvas Generative UI widget inline inside the Rursor Agent chat.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "title": { "type": "STRING", "description": "Short widget title." },
                    "html": { "type": "STRING", "description": "Self-contained HTML/CSS/JS body or document to render interactively." }
                },
                "required": ["title", "html"]
            }
        }),
        json!({
            "name": "web_search",
            "description": "Search the live web using Gemini Google Search Grounding (with keyless fallback) and return cited answers.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "query": { "type": "STRING", "description": "Web search query." },
                    "limit": { "type": "INTEGER", "description": "Max citations to return (default 6)." }
                },
                "required": ["query"]
            }
        }),
        json!({
            "name": "read_url_markdown",
            "description": "Fetch an external HTTP/HTTPS documentation page or article and convert it into clean Markdown.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "url": { "type": "STRING", "description": "HTTP/HTTPS URL to read." }
                },
                "required": ["url"]
            }
        }),
        json!({
            "name": "search_docs",
            "description": "Search persistent @docs documentation sites indexed into Rursor's hybrid FTS5 + vector store.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "query": { "type": "STRING", "description": "Documentation search query." },
                    "source": { "type": "STRING", "description": "Optional @docs source handle filter." },
                    "limit": { "type": "INTEGER", "description": "Max chunks to return (default 6)." }
                },
                "required": ["query"]
            }
        }),
    ];

    decls.extend(mcp_decls.iter().cloned());
    vec![json!({ "functionDeclarations": decls })]
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

    let current_line_prefix = prefix.rsplit('\n').next().unwrap_or("");
    if !current_line_prefix.is_empty() && text.starts_with(current_line_prefix) {
        text = text[current_line_prefix.len()..].to_string();
    }

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
