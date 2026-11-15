use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex};
use tokio::time::{timeout, Duration};

static NEXT_MCP_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Deserialize)]
struct McpConfigFile {
    #[serde(default, rename = "mcpServers")]
    mcp_servers: HashMap<String, McpServerConfig>,
}

#[derive(Debug, Deserialize, Clone)]
struct McpServerConfig {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolSpec {
    pub qualified_name: String,
    pub server_name: String,
    pub tool_name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerStatus {
    pub name: String,
    pub connected: bool,
    pub tool_count: usize,
    pub error: Option<String>,
}

struct McpConnection {
    stdin: Mutex<ChildStdin>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>,
    _child: Mutex<Child>,
}

impl McpConnection {
    async fn connect(cfg: &McpServerConfig, cwd: Option<&Path>) -> Result<Self, String> {
        let mut cmd = Command::new(&cfg.command);
        cmd.args(&cfg.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        for (k, v) in &cfg.env {
            cmd.env(k, v);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Failed to spawn MCP server '{}': {e}", cfg.command))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "MCP server missing stdin".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "MCP server missing stdout".to_string())?;

        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending_reader = Arc::clone(&pending);

        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
                    continue;
                };
                if let Some(id) = val.get("id").and_then(|v| v.as_u64()) {
                    if let Some(tx) = pending_reader.lock().await.remove(&id) {
                        if let Some(err) = val.get("error") {
                            let msg = err
                                .get("message")
                                .and_then(|v| v.as_str())
                                .unwrap_or("MCP error")
                                .to_string();
                            let _ = tx.send(Err(msg));
                        } else {
                            let result = val.get("result").cloned().unwrap_or(Value::Null);
                            let _ = tx.send(Ok(result));
                        }
                    }
                }
            }
        });

        let conn = Self {
            stdin: Mutex::new(stdin),
            pending,
            _child: Mutex::new(child),
        };

        // MCP handshake: initialize -> notifications/initialized
        let _ = conn
            .call(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "rursor-sidecar", "version": "0.2.0" }
                }),
            )
            .await?;

        conn.notify("notifications/initialized", json!({})).await?;
        Ok(conn)
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        let payload = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        });
        let mut stdin = self.stdin.lock().await;
        let line = format!("{payload}\n");
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("MCP notify write failed: {e}"))?;
        stdin
            .flush()
            .await
            .map_err(|e| format!("MCP notify flush failed: {e}"))?;
        Ok(())
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = NEXT_MCP_ID.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        {
            let mut stdin = self.stdin.lock().await;
            let line = format!("{payload}\n");
            if let Err(e) = stdin.write_all(line.as_bytes()).await {
                self.pending.lock().await.remove(&id);
                return Err(format!("MCP write failed: {e}"));
            }
            let _ = stdin.flush().await;
        }

        match timeout(Duration::from_secs(15), rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Err("MCP server closed connection".into()),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(format!("MCP call '{method}' timed out"))
            }
        }
    }
}

#[derive(Default)]
pub struct McpManager {
    connections: HashMap<String, Arc<McpConnection>>,
    tools: Vec<McpToolSpec>,
    statuses: Vec<McpServerStatus>,
}

impl McpManager {
    pub async fn reload(&mut self, workspace_root: Option<&Path>) -> Vec<McpServerStatus> {
        self.connections.clear();
        self.tools.clear();
        self.statuses.clear();

        let configs = discover_mcp_configs(workspace_root);
        for (server_name, cfg) in configs {
            match McpConnection::connect(&cfg, workspace_root).await {
                Ok(conn) => {
                    let conn_arc = Arc::new(conn);
                    match conn_arc.call("tools/list", json!({})).await {
                        Ok(res) => {
                            let list = res
                                .get("tools")
                                .and_then(|v| v.as_array())
                                .cloned()
                                .unwrap_or_default();
                            let mut count = 0usize;
                            for t in list {
                                let Some(tool_name) = t.get("name").and_then(|v| v.as_str()) else {
                                    continue;
                                };
                                let desc = t
                                    .get("description")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let input_schema = t
                                    .get("inputSchema")
                                    .cloned()
                                    .unwrap_or_else(|| json!({ "type": "OBJECT", "properties": {} }));
                                let qualified = sanitize_tool_name(&format!("mcp_{server_name}_{tool_name}"));
                                self.tools.push(McpToolSpec {
                                    qualified_name: qualified,
                                    server_name: server_name.clone(),
                                    tool_name: tool_name.to_string(),
                                    description: format!("[MCP:{server_name}] {desc}"),
                                    input_schema,
                                });
                                count += 1;
                            }
                            self.connections.insert(server_name.clone(), conn_arc);
                            self.statuses.push(McpServerStatus {
                                name: server_name,
                                connected: true,
                                tool_count: count,
                                error: None,
                            });
                        }
                        Err(e) => {
                            self.statuses.push(McpServerStatus {
                                name: server_name,
                                connected: false,
                                tool_count: 0,
                                error: Some(e),
                            });
                        }
                    }
                }
                Err(e) => {
                    self.statuses.push(McpServerStatus {
                        name: server_name,
                        connected: false,
                        tool_count: 0,
                        error: Some(e),
                    });
                }
            }
        }

        self.statuses.clone()
    }

    pub fn statuses(&self) -> Vec<McpServerStatus> {
        self.statuses.clone()
    }

    pub fn gemini_function_declarations(&self) -> Vec<Value> {
        self.tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.qualified_name,
                    "description": t.description,
                    "parameters": normalize_schema_for_gemini(&t.input_schema)
                })
            })
            .collect()
    }

    pub fn has_tool(&self, qualified_name: &str) -> bool {
        self.tools.iter().any(|t| t.qualified_name == qualified_name)
    }

    pub async fn call_tool(&self, qualified_name: &str, args: Value) -> Result<Value, String> {
        let spec = self
            .tools
            .iter()
            .find(|t| t.qualified_name == qualified_name)
            .ok_or_else(|| format!("Unknown MCP tool: {qualified_name}"))?;
        let conn = self
            .connections
            .get(&spec.server_name)
            .ok_or_else(|| format!("MCP server '{}' is not connected", spec.server_name))?;
        conn.call(
            "tools/call",
            json!({
                "name": spec.tool_name,
                "arguments": args
            }),
        )
        .await
    }
}

fn discover_mcp_configs(workspace_root: Option<&Path>) -> HashMap<String, McpServerConfig> {
    let mut merged = HashMap::new();
    let home = std::env::var("HOME").ok().map(PathBuf::from);

    if let Some(h) = home {
        load_mcp_file(&h.join(".rursor").join("mcp.json"), &mut merged);
    }
    if let Some(ws) = workspace_root {
        load_mcp_file(&ws.join(".rursor").join("mcp.json"), &mut merged);
    }
    merged
}

fn load_mcp_file(path: &Path, out: &mut HashMap<String, McpServerConfig>) {
    let Ok(raw) = fs::read_to_string(path) else {
        return;
    };
    if let Ok(parsed) = serde_json::from_str::<McpConfigFile>(&raw) {
        for (name, cfg) in parsed.mcp_servers {
            out.insert(name, cfg);
        }
    }
}

fn sanitize_tool_name(raw: &str) -> String {
    raw.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .take(64)
        .collect()
}

fn normalize_schema_for_gemini(schema: &Value) -> Value {
    let mut obj = schema.as_object().cloned().unwrap_or_default();
    obj.remove("$schema");
    obj.remove("additionalProperties");
    if let Some(t) = obj.get("type").and_then(|v| v.as_str()) {
        obj.insert("type".into(), Value::String(t.to_uppercase()));
    }
    if let Some(props) = obj.get("properties").and_then(|v| v.as_object()).cloned() {
        let mut new_props = serde_json::Map::new();
        for (k, v) in props {
            new_props.insert(k, normalize_schema_for_gemini(&v));
        }
        obj.insert("properties".into(), Value::Object(new_props));
    }
    Value::Object(obj)
}
