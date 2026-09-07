//! MCP(Model Context Protocol)stdio 客户端:把外部工具进程包成模型可调的工具。
//! 配置在 ~/.znaide/mcp.json,形如
//! {"mcpServers":{"filesystem":{"command":"node","args":["/path/server.js"]}}}。
//! 与子进程走 stdin/stdout、JSON-RPC 2.0(每行一条),
//! 流程 initialize → notifications/initialized → tools/list → tools/call。
use crate::llm::types::ToolDef;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{oneshot, Mutex as AsyncMutex};
use tokio::time::{timeout, Duration};

/// MCP 配置(与 mcp.json 对应)
#[derive(Debug, Clone, Default, Deserialize)]
pub struct McpConfig {
    #[serde(default, rename = "mcpServers")]
    pub mcp_servers: HashMap<String, McpServerDef>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct McpServerDef {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

/// 一个 MCP server 暴露的工具
#[derive(Debug, Clone)]
pub struct McpToolDef {
    pub server: String,
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// MCP 管理器:持有所有已连接 server。方法均 &self(内部锁),可安全共享。
pub struct McpManager {
    clients: Vec<McpClient>,
    /// 连接失败信息(server 名 → 错误)
    pub errors: Vec<(String, String)>,
}

#[allow(dead_code)]
pub struct McpClient {
    name: String,
    child: tokio::process::Child,
    stdin: Arc<AsyncMutex<tokio::process::ChildStdin>>,
    /// id → 响应投递
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    next_id: AtomicU64,
    tools: Vec<McpToolDef>,
}

#[derive(Serialize)]
struct Request<'a> {
    jsonrpc: &'static str,
    id: u64,
    method: &'a str,
    params: Value,
}

const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

impl McpManager {
    /// 空管理器(未配置 MCP)
    pub fn start_empty() -> Self {
        Self {
            clients: Vec::new(),
            errors: Vec::new(),
        }
    }

    /// 从 ~/.znaide/mcp.json 启动全部 server
    pub async fn start() -> Self {
        let cfg: McpConfig =
            match std::fs::read_to_string(crate::config::data_dir().join("mcp.json")) {
                Ok(text) => match serde_json::from_str(&text) {
                    Ok(c) => c,
                    Err(e) => {
                        return Self {
                            clients: Vec::new(),
                            errors: vec![("config".into(), format!("mcp.json 解析失败: {e}"))],
                        }
                    }
                },
                Err(_) => McpConfig::default(),
            };
        Self::start_with(cfg).await
    }

    pub async fn start_with(cfg: McpConfig) -> Self {
        let mut clients = Vec::new();
        let mut errors = Vec::new();
        let mut names: Vec<&String> = cfg.mcp_servers.keys().collect();
        names.sort();
        for name in names {
            let def = cfg.mcp_servers[name].clone();
            match McpClient::connect(name, &def).await {
                Ok(c) => clients.push(c),
                Err(e) => errors.push((name.clone(), format!("{e:#}"))),
            }
        }
        Self { clients, errors }
    }

    /// 合并所有 server 的工具为 ToolDef(名字带 server 前缀)
    pub fn tool_defs(&self) -> Vec<ToolDef> {
        let mut out = Vec::new();
        for c in &self.clients {
            for t in &c.tools {
                let schema = if t.input_schema.is_null() {
                    json!({"type": "object", "properties": {}})
                } else {
                    t.input_schema.clone()
                };
                out.push(ToolDef::function(
                    format!("mcp__{}__{}", c.name, t.name),
                    format!("(MCP 工具 {}/{}) {}", c.name, t.name, t.description),
                    schema,
                ));
            }
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.clients.is_empty()
    }

    /// 是否有某 MCP 工具(完整名 mcp__<server>__<tool>)
    pub fn has_tool(&self, full_name: &str) -> bool {
        let parts: Vec<&str> = full_name.splitn(3, "__").collect();
        if parts.len() != 3 || parts[0] != "mcp" {
            return false;
        }
        self.clients.iter().any(|c| {
            c.name == parts[1] && c.tools.iter().any(|t| t.name == parts[2])
        })
    }

    /// 调用 MCP 工具,返回文本结果
    pub async fn call(&self, full_name: &str, args: Value) -> anyhow::Result<String> {
        let parts: Vec<&str> = full_name.splitn(3, "__").collect();
        if parts.len() != 3 || parts[0] != "mcp" {
            anyhow::bail!("非法的 MCP 工具名: {full_name}");
        }
        for c in &self.clients {
            if c.name == parts[1] {
                return c.call_tool(parts[2], args).await;
            }
        }
        anyhow::bail!("MCP server「{}」未连接", parts[1])
    }

    pub fn client_names(&self) -> Vec<&str> {
        self.clients.iter().map(|c| c.name.as_str()).collect()
    }
}

impl McpClient {
    pub async fn connect(name: &str, def: &McpServerDef) -> anyhow::Result<Self> {
        let mut cmd = tokio::process::Command::new(&def.command);
        cmd.args(&def.args)
            .envs(&def.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("启动 {}({}): {e}", name, def.command))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("无法取得 {name} 的 stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("无法取得 {name} 的 stdout"))?;

        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> = Arc::default();
        let pending_reader = pending.clone();
        // stdout 读取任务
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        let line = line.trim();
                        if line.is_empty() {
                            continue;
                        }
                        if let Ok(v) = serde_json::from_str::<Value>(line) {
                            if let Some(id) = v.get("id").and_then(|i| i.as_u64()) {
                                if let Some(tx) = pending_reader.lock().unwrap().remove(&id) {
                                    let _ = tx.send(v);
                                }
                            }
                        }
                    }
                    Ok(None) | Err(_) => break,
                }
            }
        });

        let mut c = McpClient {
            name: name.to_string(),
            child,
            stdin: Arc::new(AsyncMutex::new(stdin)),
            pending,
            next_id: AtomicU64::new(1),
            tools: Vec::new(),
        };

        // initialize
        let init_params = json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "znaide", "version": env!("CARGO_PKG_VERSION")}
        });
        c.request("initialize", init_params).await?;
        // initialized 通知
        let notif = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        });
        c.write_line(&notif).await?;

        // tools/list
        let resp = c.request("tools/list", json!({})).await?;
        let raw_tools = resp
            .get("result")
            .and_then(|r| r.get("tools"))
            .cloned()
            .unwrap_or_else(|| json!([]));
        if let Ok(arr) = serde_json::from_value::<Vec<RawMcpTool>>(raw_tools) {
            c.tools = arr
                .into_iter()
                .map(|t| McpToolDef {
                    server: name.to_string(),
                    name: t.name,
                    description: t.description.unwrap_or_default(),
                    input_schema: t.input_schema.unwrap_or(Value::Null),
                })
                .collect();
        }
        Ok(c)
    }

    async fn write_line(&self, v: &Value) -> anyhow::Result<()> {
        let text = serde_json::to_string(v)?;
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(text.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        Ok(())
    }

    /// 发送请求并等待响应(带超时)
    async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = Request {
            jsonrpc: "2.0",
            id,
            method,
            params,
        };
        let body = serde_json::to_string(&req)?;
        {
            let mut stdin = self.stdin.lock().await;
            stdin.write_all(body.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await?;
        }
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        match timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(resp)) => {
                if let Some(err) = resp.get("error") {
                    anyhow::bail!("MCP「{}」错误: {}", self.name, err);
                }
                Ok(resp)
            }
            Ok(Err(_)) => {
                anyhow::bail!("MCP「{}」响应通道关闭(进程可能已退出)", self.name)
            }
            Err(_) => anyhow::bail!("MCP「{}」请求超时({REQUEST_TIMEOUT:?})", self.name),
        }
    }

    /// 调用工具,返回文本结果(合并 content 各 text 块)
    pub async fn call_tool(&self, tool: &str, args: Value) -> anyhow::Result<String> {
        let params = json!({
            "name": tool,
            "arguments": args
        });
        let resp = self.request("tools/call", params).await?;
        let result = resp.get("result").cloned().unwrap_or_else(|| json!({}));
        let mut text = String::new();
        if let Some(contents) = result.get("content").and_then(|c| c.as_array()) {
            for c in contents {
                if let Some(t) = c.get("text").and_then(|x| x.as_str()) {
                    text.push_str(t);
                    text.push('\n');
                }
            }
        }
        if let Some(se) = result.get("structuredContent") {
            if text.trim().is_empty() {
                text = serde_json::to_string_pretty(se)?;
            }
        }
        if text.trim().is_empty() {
            text = serde_json::to_string_pretty(&result)?;
        }
        // 检查 isError
        if result.get("isError").and_then(|x| x.as_bool()).unwrap_or(false) {
            anyhow::bail!("MCP 工具 {tool} 执行出错:\n{text}");
        }
        Ok(text)
    }

}

#[derive(Debug, Deserialize)]
struct RawMcpTool {
    name: String,
    description: Option<String>,
    #[serde(rename = "inputSchema")]
    input_schema: Option<Value>,
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
