use crate::config::Resolved;
use crate::llm::client::{http_error, new_http_client_with_proxy, truncate, LlmClient};
use crate::llm::openai::{AssistantReply, StreamEvent};
use crate::llm::sse::SseParser;
use crate::llm::types::{ChatMessage, FunctionCall, Role, ToolCall, ToolDef, Usage};
use futures_util::StreamExt;
use serde_json::{json, Value};

/// OpenAI Responses 协议客户端(`POST {base}/responses`)。
/// 内存/落盘仍是 `ChatMessage`(chat 语义);只在发请求前一刻转换、收响应时解析,
/// 对外一律返回与 `OpenAiClient` 相同的 `AssistantReply` / `StreamEvent`。
#[derive(Debug, Clone)]
pub struct ResponsesClient {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    model: String,
    /// 稳定会话头的值(Some = 开，None = 关)。空串 = 开但真实会话 ID 未到(占位，不发头)。
    session_header: Option<String>,
    /// 生效代理快照（与 base_url/model/api_key 并列的连接属性；变化即重建 http）
    proxy: crate::config::EffectiveProxy,
}

// 稳定会话头(服务端 KV 缓存路由用)，与 OpenAiClient::SESSION_HEADER 同值；
// 此处复用 openai 的常量，保持双客户端逐行对称。

// ---- 请求构造 ----

/// `ChatMessage` 逐条转 Responses `input`;首条 system 进顶层 `instructions`。
/// (instruments, input) —— 见 04 §4.1 映射表。
pub fn chat_to_response_input(messages: &[ChatMessage]) -> (Option<String>, Vec<Value>) {
    let mut instructions = None;
    let mut input = Vec::new();
    for (i, m) in messages.iter().enumerate() {
        if i == 0 && m.role == Role::System {
            instructions = m.content.clone();
            continue;
        }
        match m.role {
            Role::System => input
                .push(json!({"role": "system", "content": m.content.clone().unwrap_or_default()})),
            Role::User => input
                .push(json!({"role": "user", "content": m.content.clone().unwrap_or_default()})),
            Role::Assistant => {
                if let Some(tcs) = &m.tool_calls {
                    match &m.content {
                        Some(c) if !c.is_empty() => {
                            input.push(json!({"role": "assistant", "content": c}));
                        }
                        _ => {
                            input.push(json!({"role": "assistant"}));
                        }
                    }
                    for tc in tcs {
                        let id = if tc.id.is_empty() {
                            format!("call_{}", tc.function.name)
                        } else {
                            tc.id.clone()
                        };
                        input.push(json!({
                            "type": "function_call",
                            "call_id": id,
                            "name": tc.function.name,
                            "arguments": tc.function.arguments,
                        }));
                    }
                } else {
                    // 空 assistant(无内容无工具)不应发生:跳过(与 chat 侧"必有内容或工具"对应)
                    match &m.content {
                        Some(c) => {
                            input.push(json!({"role": "assistant", "content": c.clone()}));
                        }
                        None => continue,
                    }
                }
            }
            Role::Tool => input.push(json!({
                "type": "function_call_output",
                "call_id": m.tool_call_id.clone().unwrap_or_default(),
                "output": m.content.clone().unwrap_or_default(),
            })),
        }
    }
    (instructions, input)
}

/// `ToolDef` 转 Responses tool(`function.name → name` 平级,`parameters` 原样透传)。
pub fn tool_defs_to_response_tools(tools: &[ToolDef]) -> Value {
    Value::Array(
        tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "name": t.function.name,
                    "description": t.function.description,
                    "parameters": t.function.parameters,
                })
            })
            .collect(),
    )
}

// ---- 非流式解析 ----

/// 从 Responses JSON 顶层提取 usage(`input_tokens/output_tokens`;缺失归 0)
fn extract_response_usage(parsed: &Value) -> Usage {
    let u = parsed.get("usage");
    let n = |k: &str| {
        u.and_then(|v| v.get(k))
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    };
    Usage {
        prompt_tokens: n("input_tokens"),
        completion_tokens: n("output_tokens"),
    }
}

/// 非流式 `POST /responses` 响应解析(容错:未知 type/超集字段一律忽略)
fn parse_response_response(text: &str) -> anyhow::Result<AssistantReply> {
    let parsed: Value = serde_json::from_str(text)
        .map_err(|e| anyhow::anyhow!("解析模型响应失败: {e}\n原始内容: {}", truncate(text, 500)))?;
    match parsed.get("status").and_then(|s| s.as_str()) {
        Some("failed") => {
            let msg = parsed
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("未知错误");
            anyhow::bail!("模型响应失败: {msg}");
        }
        Some("incomplete") => {
            anyhow::bail!("模型输出超过上下文长度被截断(length)");
        }
        _ => {}
    }
    let output = parsed
        .get("output")
        .and_then(|o| o.as_array())
        .ok_or_else(|| anyhow::anyhow!("模型响应缺少 output"))?;
    Ok(response_output_to_reply(
        output,
        extract_response_usage(&parsed),
    ))
}

fn response_output_to_reply(output: &[Value], usage: Usage) -> AssistantReply {
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    for item in output {
        match item.get("type").and_then(|t| t.as_str()) {
            Some("message") => match item.get("content") {
                // 兼容:content 为字符串时直接收
                Some(Value::String(s)) => content.push_str(s),
                Some(Value::Array(parts)) => {
                    for p in parts {
                        let t = p.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        // 标准 `output_text`,部分兼容端点用 `text`
                        if t == "output_text" || t == "text" {
                            if let Some(s) = p.get("text").and_then(|t| t.as_str()) {
                                content.push_str(s);
                            }
                        }
                    }
                }
                _ => {}
            },
            Some("function_call") => {
                if let Some(tc) = response_item_to_tool_call(item) {
                    tool_calls.push(tc);
                }
            }
            Some("reasoning") => {
                if let Some(arr) = item.get("summary").and_then(|s| s.as_array()) {
                    for s in arr {
                        if let Some(t) = s.get("text").and_then(|t| t.as_str()) {
                            reasoning.push_str(t);
                        }
                    }
                }
            }
            // 未知 type:向前兼容,直接忽略
            _ => {}
        }
    }
    AssistantReply {
        content: if content.is_empty() {
            None
        } else {
            Some(content)
        },
        reasoning_content: if reasoning.is_empty() {
            None
        } else {
            Some(reasoning)
        },
        tool_calls,
        usage,
    }
}

/// 单条 `function_call` item 转内部 ToolCall;缺函数名/空名则丢弃(与 chat 的
/// `value_to_tool_call` 同规则)。
fn response_item_to_tool_call(item: &Value) -> Option<ToolCall> {
    let name = item.get("name").and_then(|n| n.as_str())?;
    if name.is_empty() {
        return None;
    }
    // arguments:字符串原样保留;对象/其他 → 序列化回 JSON 字符串
    let arguments = match item.get("arguments") {
        None | Some(Value::Null) => "{}".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    };
    let id = item
        .get("call_id")
        .and_then(|i| i.as_str())
        .filter(|i| !i.is_empty())
        .map(|i| i.to_string())
        .or_else(|| {
            item.get("id")
                .and_then(|i| i.as_str())
                .filter(|i| !i.is_empty())
                .map(|i| i.to_string())
        })
        .unwrap_or_else(|| format!("call_{name}"));
    Some(ToolCall {
        id,
        call_type: "function".to_string(),
        function: FunctionCall {
            name: name.to_string(),
            arguments,
        },
    })
}

// ---- 流式 ----

/// 流式 function_call 累积器(按 `output_index` 聚合;`name`/`call_id` 在
/// `output_item.added` 里先到,arguments 经多片 delta 拼接)。
/// message 槽天然无 name,被 `finish()` 过滤掉。
#[derive(Debug, Clone, Default)]
struct FunctionCallAccumulator {
    calls: Vec<AccToolCall>,
}

#[derive(Debug, Clone, Default)]
struct AccToolCall {
    id: String,
    call_id: String,
    name: String,
    arguments: String,
}

impl FunctionCallAccumulator {
    fn ensure(&mut self, output_index: usize) {
        while self.calls.len() <= output_index {
            self.calls.push(AccToolCall::default());
        }
    }

    /// `response.output_item.added` 登记槽位(id/name 先到)
    fn set_item(&mut self, output_index: usize, item: &Value) {
        if item.get("type").and_then(|t| t.as_str()) != Some("function_call") {
            return;
        }
        self.ensure(output_index);
        let c = &mut self.calls[output_index];
        if c.id.is_empty() {
            if let Some(id) = item
                .get("call_id")
                .and_then(|i| i.as_str())
                .filter(|i| !i.is_empty())
                .or_else(|| {
                    item.get("id")
                        .and_then(|i| i.as_str())
                        .filter(|i| !i.is_empty())
                })
            {
                c.id = id.to_string();
            }
        }
        if c.call_id.is_empty() {
            if let Some(call_id) = item
                .get("call_id")
                .and_then(|i| i.as_str())
                .filter(|i| !i.is_empty())
            {
                c.call_id = call_id.to_string();
            }
        }
        if c.name.is_empty() {
            if let Some(n) = item.get("name").and_then(|v| v.as_str()) {
                if !n.is_empty() {
                    c.name = n.to_string();
                }
            }
        }
    }

    /// `response.function_call_arguments.delta` 拼 arguments 片(`item_id` 仅做
    /// 一致性参考,定位以 `output_index` 为准)
    fn push_delta(&mut self, output_index: usize, delta: &str) {
        self.ensure(output_index);
        self.calls[output_index].arguments.push_str(delta);
    }

    /// `response.output_item.done` 用完整体补齐仍缺的 `name`/`id`
    fn finish_item(&mut self, output_index: usize, item: &Value) {
        if item.get("type").and_then(|t| t.as_str()) != Some("function_call") {
            return;
        }
        self.ensure(output_index);
        let c = &mut self.calls[output_index];
        if c.name.is_empty() {
            if let Some(n) = item.get("name").and_then(|v| v.as_str()) {
                if !n.is_empty() {
                    c.name = n.to_string();
                }
            }
        }
        if c.id.is_empty() {
            if let Some(id) = item
                .get("call_id")
                .and_then(|i| i.as_str())
                .filter(|i| !i.is_empty())
                .or_else(|| {
                    item.get("id")
                        .and_then(|i| i.as_str())
                        .filter(|i| !i.is_empty())
                })
            {
                c.id = id.to_string();
            }
        }
        if c.call_id.is_empty() {
            if let Some(call_id) = item
                .get("call_id")
                .and_then(|i| i.as_str())
                .filter(|i| !i.is_empty())
            {
                c.call_id = call_id.to_string();
            }
        }
    }

    fn finish(self) -> Vec<ToolCall> {
        self.calls
            .into_iter()
            .filter(|c| !c.name.is_empty())
            .map(|c| {
                let id = if !c.call_id.is_empty() {
                    c.call_id
                } else if !c.id.is_empty() {
                    c.id
                } else {
                    format!("call_{}", c.name)
                };
                ToolCall {
                    id,
                    call_type: "function".into(),
                    function: FunctionCall {
                        name: c.name,
                        arguments: c.arguments,
                    },
                }
            })
            .collect()
    }
}

/// 处理一条 Responses SSE 事件负载。返回 true 仅当 `response.completed`
/// (流结束标志;服务端也可能直接断流,此时以已累积内容收尾)。
fn apply_response_event(
    line: &str,
    content: &mut String,
    reasoning: &mut String,
    usage: &mut Usage,
    acc: &mut FunctionCallAccumulator,
    on_event: &mut (dyn FnMut(StreamEvent) + Send),
) -> Result<bool, anyhow::Error> {
    let parsed: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return Ok(false), // 心跳/注释行,不中断流
    };
    let ty = parsed.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match ty {
        "response.output_text.delta" => {
            if let Some(d) = parsed.get("delta").and_then(|d| d.as_str()) {
                if !d.is_empty() {
                    content.push_str(d);
                    on_event(StreamEvent::TextDelta(d.to_string()));
                }
            }
        }
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            if let Some(d) = parsed.get("delta").and_then(|d| d.as_str()) {
                if !d.is_empty() {
                    reasoning.push_str(d);
                    on_event(StreamEvent::ReasoningDelta(d.to_string()));
                }
            }
        }
        "response.function_call_arguments.delta" => {
            let idx = parsed
                .get("output_index")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            if let Some(d) = parsed.get("delta").and_then(|d| d.as_str()) {
                acc.push_delta(idx, d);
            }
        }
        "response.output_item.added" => {
            let idx = parsed
                .get("output_index")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            if let Some(item) = parsed.get("item") {
                acc.set_item(idx, item);
            }
        }
        "response.output_item.done" => {
            let idx = parsed
                .get("output_index")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            if let Some(item) = parsed.get("item") {
                acc.finish_item(idx, item);
            }
        }
        "response.completed" => {
            if let Some(u) = parsed.get("response").and_then(|r| r.get("usage")) {
                let n = |k: &str| u.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
                let got = Usage {
                    prompt_tokens: n("input_tokens"),
                    completion_tokens: n("output_tokens"),
                };
                if !got.is_empty() {
                    *usage = got;
                }
            }
            return Ok(true);
        }
        "response.failed" => {
            let msg = parsed
                .get("response")
                .and_then(|r| r.get("error"))
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("未知错误");
            anyhow::bail!("模型响应失败: {msg}");
        }
        "response.incomplete" => {
            anyhow::bail!("模型输出超过上下文长度被截断(length)");
        }
        // created / in_progress / 未知:忽略
        _ => {}
    }
    Ok(false)
}

// ---- 客户端 ----

impl ResponsesClient {
    pub fn new(cfg: &Resolved) -> anyhow::Result<Self> {
        let http = new_http_client_with_proxy(cfg.proxy.url())?;
        Ok(Self {
            http,
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            api_key: cfg.api_key.clone(),
            model: cfg.model.clone(),
            // 开但真实会话 ID 未到(Session 建好后经 set_session_header 回填);
            // 空串占位时 auth() 不发头
            session_header: if cfg.session_header_enabled {
                Some(String::new())
            } else {
                None
            },
            proxy: cfg.proxy.clone(),
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// 运行时换端点/key/模型/代理(代理是 Builder 期 baked，换出口必须重建 http，连接池不复用)
    pub fn reconfigure(&mut self, cfg: &Resolved) {
        self.base_url = cfg.base_url.trim_end_matches('/').to_string();
        self.model = cfg.model.clone();
        self.api_key = cfg.api_key.clone();
        // 开关关 → 清掉旧值(任何路径不得残留);开但已有值 → 保留;开且无值 → 占位等回填
        match (cfg.session_header_enabled, self.session_header.take()) {
            (false, _) => self.session_header = None,
            (true, Some(v)) if !v.is_empty() => self.session_header = Some(v),
            (true, _) => self.session_header = Some(String::new()),
        }
        if self.proxy != cfg.proxy {
            // 代理是 Builder 期 baked，换出口必须重建 client（连接池不复用）；
            // 重建失败（非法 URL 漏网）保留旧 client 安全降级
            match new_http_client_with_proxy(cfg.proxy.url()) {
                Ok(http) => {
                    self.http = http;
                    self.proxy = cfg.proxy.clone();
                }
                Err(e) => eprintln!("⚠ 代理切换时重建客户端失败({e:#})，仍用旧代理继续"),
            }
        }
    }

    /// Session 建好/恢复/重配后调用:值 = session_id;关 → 清掉。
    pub fn set_session_header(&mut self, enabled: bool, session_id: &str) {
        self.session_header = if enabled {
            Some(session_id.to_string())
        } else {
            None
        };
    }

    fn url(&self) -> String {
        format!("{}/responses", self.base_url)
    }

    fn models_url(&self) -> String {
        format!("{}/models", self.base_url)
    }

    /// 查该端点可用模型列表(GET /models,与协议无关)
    pub async fn list_models(&self) -> anyhow::Result<Vec<String>> {
        let resp = self.auth(self.http.get(self.models_url())).send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            anyhow::bail!("模型列表查询失败(HTTP {status}): {}", truncate(&text, 300));
        }
        let parsed: Value = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("解析模型列表失败: {e}\n{}", truncate(&text, 300)))?;
        let mut out: Vec<String> = parsed
            .get("data")
            .and_then(|d| d.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// 连通性验证:最小 Responses 请求(经转换得到 `input:[{role:user,content:"hi"}]`)。
    /// 有内容/无内容但成功都算可用,只看请求本身成不成。
    pub async fn validate(&self) -> anyhow::Result<()> {
        let messages = vec![ChatMessage::user("hi")];
        self.chat(&messages, None).await?;
        Ok(())
    }

    /// 鉴权 + 稳定会话头:唯一注头点(与 OpenAiClient::auth 同构)。
    /// 头名复用 openai 的 SESSION_HEADER 常量，双客户端发出的头完全一致。
    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let req = match &self.api_key {
            Some(k) => req.bearer_auth(k),
            None => req,
        };
        match &self.session_header {
            Some(v) if !v.is_empty() => match reqwest::header::HeaderValue::from_str(v) {
                Ok(hv) => req.header(crate::llm::openai::SESSION_HEADER, hv),
                Err(_) => {
                    eprintln!("⚠ 会话头值非法，已跳过（不影响本次请求）");
                    req
                }
            },
            _ => req,
        }
    }

    /// 组装 Responses 请求体(`model/instructions/input/tools/stream`;首版不发
    /// `previous_response_id/store/reasoning/max_output_tokens`,与 chat 侧对齐)。
    fn build_response_body(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolDef]>,
        stream: bool,
    ) -> anyhow::Result<serde_json::Map<String, Value>> {
        let (instructions, input) = chat_to_response_input(messages);
        let mut body = serde_json::Map::new();
        body.insert("model".into(), json!(self.model));
        if let Some(ins) = instructions {
            body.insert("instructions".into(), json!(ins));
        }
        body.insert("input".into(), Value::Array(input));
        // Responses 的 usage 随 `response.completed` 一起回,不发 `stream_options`
        body.insert("stream".into(), json!(stream));
        if let Some(tools) = tools {
            body.insert("tools".into(), tool_defs_to_response_tools(tools));
        }
        Ok(body)
    }

    /// 发起一次非流式 Responses 调用;`tools` 为 None 时不带工具声明
    pub async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolDef]>,
    ) -> anyhow::Result<AssistantReply> {
        let body = self.build_response_body(messages, tools, false)?;
        let resp = self
            .auth(self.http.post(self.url()))
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(http_error(status, &text));
        }
        parse_response_response(&text)
    }

    /// 流式 Responses 调用:SSE 增量经 on_event 回调逐条送出,结束返回累积的完整回复。
    /// 流结束标志是 `response.completed`(没有 `[DONE]`);服务端直接断流时以已累积内容收尾。
    pub async fn chat_stream<F>(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolDef]>,
        mut on_event: F,
    ) -> anyhow::Result<AssistantReply>
    where
        F: FnMut(StreamEvent) + Send,
    {
        let body = self.build_response_body(messages, tools, true)?;

        let resp = self
            .auth(self.http.post(self.url()))
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await?;
            return Err(http_error(status, &text));
        }

        let mut content = String::new();
        let mut reasoning = String::new();
        let mut usage = Usage::default();
        let mut acc = FunctionCallAccumulator::default();
        let mut sse = SseParser::new();
        let mut stream = resp.bytes_stream();

        let mut done = false;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            for line in sse.push(&chunk) {
                if apply_response_event(
                    &line,
                    &mut content,
                    &mut reasoning,
                    &mut usage,
                    &mut acc,
                    &mut on_event,
                )? {
                    done = true;
                    break;
                }
            }
            if done {
                break;
            }
        }
        // 流结束:冲刷残余(个别服务端不补最后的空行)
        if !done {
            for line in sse.finish() {
                if apply_response_event(
                    &line,
                    &mut content,
                    &mut reasoning,
                    &mut usage,
                    &mut acc,
                    &mut on_event,
                )? {
                    break;
                }
            }
        }

        Ok(AssistantReply {
            content: if content.is_empty() {
                None
            } else {
                Some(content)
            },
            reasoning_content: if reasoning.is_empty() {
                None
            } else {
                Some(reasoning)
            },
            tool_calls: acc.finish(),
            usage,
        })
    }
}

/// `LlmClient` 薄转发(与 OpenAiClient 同构)
impl LlmClient for ResponsesClient {
    fn model(&self) -> &str {
        ResponsesClient::model(self)
    }

    fn reconfigure(&mut self, cfg: &Resolved) {
        ResponsesClient::reconfigure(self, cfg)
    }

    fn set_session_header(&mut self, enabled: bool, session_id: &str) {
        ResponsesClient::set_session_header(self, enabled, session_id)
    }

    fn effective_proxy(&self) -> crate::config::EffectiveProxy {
        self.proxy.clone()
    }

    fn chat<'a>(
        &'a self,
        messages: &'a [ChatMessage],
        tools: Option<&'a [ToolDef]>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<AssistantReply>> + Send + 'a>,
    > {
        Box::pin(ResponsesClient::chat(self, messages, tools))
    }

    fn chat_stream<'a>(
        &'a self,
        messages: &'a [ChatMessage],
        tools: Option<&'a [ToolDef]>,
        on_event: &'a mut (dyn FnMut(StreamEvent) + Send),
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<AssistantReply>> + Send + 'a>,
    > {
        Box::pin(ResponsesClient::chat_stream(
            self, messages, tools, on_event,
        ))
    }

    fn validate<'a>(
        &'a self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(ResponsesClient::validate(self))
    }

    fn list_models<'a>(
        &'a self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<Vec<String>>> + Send + 'a>>
    {
        Box::pin(ResponsesClient::list_models(self))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_call(id: &str, name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: name.into(),
                arguments: args.into(),
            },
        }
    }

    #[test]
    fn input_convert_basic() {
        // 首条 system 进 instructions,不进 input
        let msgs = vec![
            ChatMessage::system("你是助手"),
            ChatMessage::user("你好"),
            ChatMessage::assistant("好的"),
        ];
        let (ins, input) = chat_to_response_input(&msgs);
        assert_eq!(ins.as_deref(), Some("你是助手"));
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"], "你好");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[1]["content"], "好的");
    }

    #[test]
    fn input_convert_tool_chain() {
        // assistant(tool_calls) → assistant 项 + function_call 项;tool → function_call_output
        let msgs = vec![
            ChatMessage::user("看看"),
            ChatMessage::assistant_with_tool_calls(
                None,
                vec![tool_call("call_1", "read_file", r#"{"path":"/tmp/a"}"#)],
            ),
            ChatMessage::tool("call_1", "文件内容"),
        ];
        let (ins, input) = chat_to_response_input(&msgs);
        assert!(ins.is_none());
        assert_eq!(input.len(), 4);
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["call_id"], "call_1");
        assert_eq!(input[2]["name"], "read_file");
        assert_eq!(input[2]["arguments"], r#"{"path":"/tmp/a"}"#);
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], "call_1");
        assert_eq!(input[3]["output"], "文件内容");
    }

    #[test]
    fn input_convert_empty_content() {
        // assistant 有 tool_calls 无 content → 省略 content 键
        let msgs = vec![ChatMessage::assistant_with_tool_calls(
            None,
            vec![tool_call("c1", "list_directory", "{}")],
        )];
        let (_, input) = chat_to_response_input(&msgs);
        assert_eq!(input[0]["role"], "assistant");
        assert!(input[0].get("content").is_none());
        assert_eq!(input[1]["type"], "function_call");
    }

    #[test]
    fn tool_defs_convert() {
        let defs = vec![ToolDef::function(
            "list_directory",
            "列目录",
            json!({"type": "object", "properties": {}, "required": ["path"]}),
        )];
        let v = tool_defs_to_response_tools(&defs);
        assert_eq!(v[0]["type"], "function");
        assert_eq!(v[0]["name"], "list_directory");
        assert_eq!(v[0]["description"], "列目录");
        assert_eq!(v[0]["parameters"]["required"], json!(["path"]));
        // 平级 name,不包 function
        assert!(v[0].get("function").is_none());
    }

    #[test]
    fn non_stream_parses_message_and_function_call() {
        let text = r#"{
            "id": "resp_123", "status": "completed",
            "output": [
                {"type": "message", "content": [{"type": "output_text", "text": "已归档"}]},
                {"type": "function_call", "call_id": "call_1", "name": "list_directory",
                 "arguments": "{\"path\":\"x\"}"}
            ],
            "usage": {"input_tokens": 120, "output_tokens": 15, "total_tokens": 135}
        }"#;
        let reply = parse_response_response(text).unwrap();
        assert_eq!(reply.content.as_deref(), Some("已归档"));
        assert_eq!(reply.tool_calls.len(), 1);
        assert_eq!(reply.tool_calls[0].id, "call_1");
        assert_eq!(reply.tool_calls[0].function.name, "list_directory");
        assert_eq!(reply.usage.prompt_tokens, 120);
        assert_eq!(reply.usage.completion_tokens, 15);
        assert_eq!(reply.usage.total(), 135);
    }

    #[test]
    fn non_stream_tolerates_object_arguments() {
        let text = r#"{"status": "completed", "output": [
            {"type": "function_call", "call_id": "c1", "name": "run_shell_command",
             "arguments": {"command": "ls -la"}}
        ]}"#;
        let reply = parse_response_response(text).unwrap();
        assert!(reply.tool_calls[0]
            .function
            .arguments
            .contains("\"command\":\"ls -la\""));
    }

    #[test]
    fn non_stream_missing_name_dropped() {
        // 无名 function_call 被丢弃;空 output → content None + 空 tool_calls,不 panic
        let text = r#"{"status": "completed", "output": [
            {"type": "function_call", "call_id": "c9", "arguments": "{}"},
            {"type": "message", "content": [{"type": "output_text", "text": "hi"}]}
        ]}"#;
        let reply = parse_response_response(text).unwrap();
        assert!(reply.tool_calls.is_empty());
        assert_eq!(reply.content.as_deref(), Some("hi"));

        let empty = r#"{"status": "completed", "output": []}"#;
        let reply = parse_response_response(empty).unwrap();
        assert!(reply.content.is_none());
        assert!(reply.tool_calls.is_empty());
    }

    #[test]
    fn non_stream_failed_status_bails() {
        let text = r#"{"status": "failed", "error": {"message": "bad key"}, "output": []}"#;
        let err = parse_response_response(text).unwrap_err();
        assert!(err.to_string().contains("bad key"), "got: {err}");
    }

    #[test]
    fn non_stream_incomplete_bails_truncated() {
        let text = r#"{"status": "incomplete", "output": []}"#;
        let err = parse_response_response(text).unwrap_err();
        assert!(err.to_string().contains("截断(length)"), "got: {err}");
    }

    #[test]
    fn non_stream_reasoning_and_string_content() {
        // reasoning 摘要 + 字符串形 content 兼容
        let text = r#"{"status": "completed", "output": [
            {"type": "reasoning", "summary": [{"type": "summary_text", "text": "思考中"}]},
            {"type": "message", "content": "直接字符串"}
        ]}"#;
        let reply = parse_response_response(text).unwrap();
        assert_eq!(reply.reasoning_content.as_deref(), Some("思考中"));
        assert_eq!(reply.content.as_deref(), Some("直接字符串"));
    }

    #[test]
    fn stream_text_and_usage() {
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut usage = Usage::default();
        let mut acc = FunctionCallAccumulator::default();
        let mut events: Vec<StreamEvent> = Vec::new();
        let mut ev = |e: StreamEvent| events.push(e);

        let done = apply_response_event(
            r#"{"type":"response.created","response":{"id":"resp_1"}}"#,
            &mut content,
            &mut reasoning,
            &mut usage,
            &mut acc,
            &mut ev,
        )
        .unwrap();
        assert!(!done);
        let done = apply_response_event(
            r#"{"type":"response.output_text.delta","output_index":0,"delta":"你好"}"#,
            &mut content,
            &mut reasoning,
            &mut usage,
            &mut acc,
            &mut ev,
        )
        .unwrap();
        assert!(!done);
        assert_eq!(content, "你好");
        // 没有 [DONE];completed 带 usage 结束
        let done = apply_response_event(
            r#"{"type":"response.completed","response":{"usage":{"input_tokens":30,"output_tokens":2}}}"#,
            &mut content,
            &mut reasoning,
            &mut usage,
            &mut acc,
            &mut ev,
        )
        .unwrap();
        assert!(done);
        assert_eq!(usage.prompt_tokens, 30);
        assert_eq!(usage.completion_tokens, 2);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], StreamEvent::TextDelta(_)));
    }

    #[test]
    fn stream_function_call_split_across_events() {
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut usage = Usage::default();
        let mut acc = FunctionCallAccumulator::default();
        let mut ev = |_: StreamEvent| {};

        for line in [
            r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read_file","arguments":""}}"#,
            r#"{"type":"response.function_call_arguments.delta","output_index":1,"item_id":"fc_1","delta":"{\"path\":"}"#,
            r#"{"type":"response.function_call_arguments.delta","output_index":1,"item_id":"fc_1","delta":"\"/tmp/a\"}"}"#,
            r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","call_id":"call_1","name":"read_file","arguments":"{\"path\":\"/tmp/a\"}"}}"#,
        ] {
            let done = apply_response_event(
                line,
                &mut content,
                &mut reasoning,
                &mut usage,
                &mut acc,
                &mut ev,
            )
            .unwrap();
            assert!(!done);
        }
        let calls = acc.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].function.name, "read_file");
        assert_eq!(calls[0].function.arguments, r#"{"path":"/tmp/a"}"#);
    }

    #[test]
    fn stream_ignores_lifecycle_events() {
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut usage = Usage::default();
        let mut acc = FunctionCallAccumulator::default();
        let mut events: Vec<StreamEvent> = Vec::new();
        let mut ev = |e: StreamEvent| events.push(e);

        for line in [
            r#"{"type":"response.created","response":{"id":"r"}}"#,
            r#"{"type":"response.in_progress","response":{"id":"r"}}"#,
            r#"{"type":"response.whatever_new","x":1}"#,
            r#"not json at all"#,
        ] {
            let done = apply_response_event(
                line,
                &mut content,
                &mut reasoning,
                &mut usage,
                &mut acc,
                &mut ev,
            )
            .unwrap();
            assert!(!done);
        }
        assert!(content.is_empty());
        assert!(events.is_empty());
    }

    #[test]
    fn stream_failed_bails() {
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut usage = Usage::default();
        let mut acc = FunctionCallAccumulator::default();
        let mut ev = |_: StreamEvent| {};
        let err = apply_response_event(
            r#"{"type":"response.failed","response":{"error":{"message":"boom"}}}"#,
            &mut content,
            &mut reasoning,
            &mut usage,
            &mut acc,
            &mut ev,
        )
        .unwrap_err();
        assert!(err.to_string().contains("boom"), "got: {err}");
    }

    #[test]
    fn accumulator_filters_message_slots() {
        // message 槽(无 name)被 finish() 过滤,只剩 function_call
        let mut acc = FunctionCallAccumulator::default();
        acc.ensure(0); // message 槽:从未登记 name
        acc.set_item(
            1,
            &json!({"type": "function_call", "call_id": "c1", "name": "read_file"}),
        );
        acc.push_delta(1, "{}");
        let calls = acc.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "read_file");
    }

    #[test]
    fn reasoning_delta_names_both_accepted() {
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut usage = Usage::default();
        let mut acc = FunctionCallAccumulator::default();
        let mut events: Vec<StreamEvent> = Vec::new();
        let mut ev = |e: StreamEvent| events.push(e);
        for line in [
            r#"{"type":"response.reasoning_summary_text.delta","delta":"想"}"#,
            r#"{"type":"response.reasoning_text.delta","delta":"想2"}"#,
        ] {
            apply_response_event(
                line,
                &mut content,
                &mut reasoning,
                &mut usage,
                &mut acc,
                &mut ev,
            )
            .unwrap();
        }
        assert_eq!(reasoning, "想想2");
        assert_eq!(events.len(), 2);
    }

    fn test_cfg(enabled: bool) -> Resolved {
        Resolved {
            model: "m".into(),
            base_url: "http://localhost:11434/v1".into(),
            api_key: None,
            provider_name: "t".into(),
            context_window: None,
            protocol: crate::config::ProtocolKind::Response,
            session_header_enabled: enabled,
            retry: crate::config::RetryConfig::disabled(),
            proxy: crate::config::EffectiveProxy::Direct,
        }
    }

    fn built_headers(c: &ResponsesClient) -> reqwest::header::HeaderMap {
        c.auth(c.http.get("http://localhost:11434/v1/models"))
            .build()
            .unwrap()
            .headers()
            .clone()
    }

    #[test]
    fn auth_session_header_off_placeholder_and_on() {
        use crate::llm::openai::SESSION_HEADER;
        // 关 → 无头
        let c = ResponsesClient::new(&test_cfg(false)).unwrap();
        assert!(built_headers(&c).get(SESSION_HEADER).is_none());
        // 开但占位(空串，值未到) → 不发
        let c = ResponsesClient::new(&test_cfg(true)).unwrap();
        assert!(built_headers(&c).get(SESSION_HEADER).is_none());
        // 开 + 回填 → 发头且值相等
        let mut c = ResponsesClient::new(&test_cfg(true)).unwrap();
        c.set_session_header(true, "abc123");
        assert_eq!(built_headers(&c).get(SESSION_HEADER).unwrap(), "abc123");
        // reconfigure 关 → 清掉旧值，不残留
        c.reconfigure(&test_cfg(false));
        assert!(built_headers(&c).get(SESSION_HEADER).is_none());
        // reconfigure 开(已有值) → 保留旧值
        c.set_session_header(true, "keep");
        c.reconfigure(&test_cfg(true));
        assert_eq!(built_headers(&c).get(SESSION_HEADER).unwrap(), "keep");
        // 非法值(含换行) → 跳过头发请求不断
        let mut c2 = ResponsesClient::new(&test_cfg(true)).unwrap();
        c2.set_session_header(true, "bad\nvalue");
        assert!(built_headers(&c2).get(SESSION_HEADER).is_none());
    }
}
