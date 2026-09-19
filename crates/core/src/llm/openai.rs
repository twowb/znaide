use crate::config::Resolved;
use crate::llm::client::{http_error, new_http_client_with_proxy, truncate, LlmClient};
use crate::llm::sse::SseParser;
use crate::llm::types::{ChatMessage, FunctionCall, ToolCall, ToolDef, Usage};
use futures_util::StreamExt;
use serde_json::json;

/// OpenAI 兼容 `/chat/completions` 客户端;换 base_url 就能接 ollama/vLLM/DeepSeek/各家兼容端点
#[derive(Debug, Clone)]
pub struct OpenAiClient {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    model: String,
    /// 稳定会话头的值(Some = 开，None = 关)。空串 = 开但真实会话 ID 未到(占位，不发头)。
    session_header: Option<String>,
    /// 生效代理快照（与 base_url/model/api_key 并列的连接属性；变化即重建 http）
    proxy: crate::config::EffectiveProxy,
}

/// 请求 user 标识:chat 请求顶层 user 字段(遥测/实例区分用)
const REQ_USER: &str = "6ba93f8a8d2e";

/// 稳定会话头(服务端 KV 缓存路由用)，开关关 → 不发。将来若做通用头表，
/// 在此常量处扩成循环注入(会话头优先，避免被覆盖)。
pub const SESSION_HEADER: &str = "x-opencode-session";

/// 向导探针头值前缀(验证阶段尚无正式 Session，用一次性探针值走通缓存路由；
/// 正式建会话后一律换成真实 session_id)。
pub fn probe_session_value() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("wizard-{ms:x}")
}

/// 模型单次回复(非流式或流式累积后的完整结果)
#[derive(Debug, Clone)]
pub struct AssistantReply {
    pub content: Option<String>,
    pub reasoning_content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    /// 本次调用的真实 token 用量(服务端 usage;不提供时为 0)
    pub usage: Usage,
}

/// 流式过程中的增量事件
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// 正文增量
    TextDelta(String),
    /// 思考内容增量(部分兼容端点如 DeepSeek / Qwen 思考模式)
    ReasoningDelta(String),
}

// ---- 模型响应解析(容错) ----
// 各家"OpenAI 兼容"端点差异很大:arguments 可能直接给对象,content 可能是数组,
// 流式 tool_calls 可能分片也可能是完整对象。下面统一做容错。

/// 从响应 JSON 顶层提取 usage(容错:缺字段/非数值一律归 0)
fn extract_usage(parsed: &serde_json::Value) -> Usage {
    let u = parsed.get("usage");
    let n = |k: &str| {
        u.and_then(|v| v.get(k))
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    };
    Usage {
        prompt_tokens: n("prompt_tokens"),
        completion_tokens: n("completion_tokens"),
    }
}

/// 把非流式响应文本解析成 AssistantReply
fn parse_chat_response(text: &str) -> anyhow::Result<AssistantReply> {
    let parsed: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| anyhow::anyhow!("解析模型响应失败: {e}\n原始内容: {}", truncate(text, 500)))?;
    let msg = parsed
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|ch| ch.get("message"))
        .ok_or_else(|| anyhow::anyhow!("模型响应缺少 choices[0].message"))?;
    Ok(AssistantReply {
        content: msg.get("content").and_then(content_text),
        reasoning_content: msg
            .get("reasoning_content")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        tool_calls: msg
            .get("tool_calls")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(value_to_tool_call).collect())
            .unwrap_or_default(),
        usage: extract_usage(&parsed),
    })
}

/// content 字段容错:纯字符串,或 {type:text}/字符串 组成的数组
fn content_text(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        serde_json::Value::Array(items) => {
            let mut out = String::new();
            for it in items {
                match it {
                    serde_json::Value::String(s) => out.push_str(s),
                    serde_json::Value::Object(m) => {
                        if let Some(t) = m.get("text").and_then(|t| t.as_str()) {
                            out.push_str(t);
                        }
                    }
                    _ => {}
                }
            }
            if out.trim().is_empty() {
                None
            } else {
                Some(out)
            }
        }
        _ => None,
    }
}

/// 把一条 tool_call(JSON 值)转成内部 ToolCall;缺函数名等残缺条目直接丢弃
fn value_to_tool_call(v: &serde_json::Value) -> Option<ToolCall> {
    let function = v.get("function")?;
    let name = function.get("name").and_then(|n| n.as_str())?;
    if name.is_empty() {
        return None;
    }
    // arguments:字符串原样保留;对象/其他 → 序列化回 JSON 字符串
    let arguments = match function.get("arguments") {
        None | Some(serde_json::Value::Null) => "{}".to_string(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    };
    let id = v
        .get("id")
        .and_then(|i| i.as_str())
        .filter(|i| !i.is_empty())
        .map(|i| i.to_string())
        .unwrap_or_else(|| format!("call_{name}"));
    let call_type = v
        .get("type")
        .and_then(|t| t.as_str())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .unwrap_or_else(|| "function".to_string());
    Some(ToolCall {
        id,
        call_type,
        function: FunctionCall {
            name: name.to_string(),
            arguments,
        },
    })
}

/// 流式工具调用累积器(按 index 合并增量)
#[derive(Debug, Clone, Default)]
struct ToolCallAccumulator {
    calls: Vec<AccToolCall>,
}

#[derive(Debug, Clone, Default)]
struct AccToolCall {
    id: String,
    call_type: String,
    name: String,
    arguments: String,
}

impl ToolCallAccumulator {
    /// 合并一条流式增量(index 定位;id/type/name 只在首片出现)
    fn merge_value(&mut self, tc: &serde_json::Value) {
        let index = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        while self.calls.len() <= index {
            self.calls.push(AccToolCall::default());
        }
        let c = &mut self.calls[index];
        if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
            if !id.is_empty() {
                c.id = id.to_string();
            }
        }
        if let Some(t) = tc.get("type").and_then(|v| v.as_str()) {
            if !t.is_empty() {
                c.call_type = t.to_string();
            }
        }
        let Some(function) = tc.get("function") else {
            return;
        };
        if let Some(n) = function.get("name").and_then(|v| v.as_str()) {
            if !n.is_empty() {
                c.name = n.to_string();
            }
        }
        if let Some(a) = function.get("arguments") {
            match a {
                serde_json::Value::String(s) => c.arguments.push_str(s),
                // 部分端点一次性给出完整对象参数 → 直接序列化整段
                _ => c.arguments.push_str(&a.to_string()),
            }
        }
    }

    fn finish(self) -> Vec<ToolCall> {
        self.calls
            .into_iter()
            .filter(|c| !c.name.is_empty())
            .map(|c| ToolCall {
                id: if c.id.is_empty() {
                    format!("call_{}", c.name)
                } else {
                    c.id
                },
                call_type: if c.call_type.is_empty() {
                    "function".into()
                } else {
                    c.call_type
                },
                function: FunctionCall {
                    name: c.name,
                    arguments: c.arguments,
                },
            })
            .collect()
    }
}

impl OpenAiClient {
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
        format!("{}/chat/completions", self.base_url)
    }

    fn models_url(&self) -> String {
        format!("{}/models", self.base_url)
    }

    /// 查该端点可用模型列表(GET /models,配置向导里自动补全模型名用)
    pub async fn list_models(&self) -> anyhow::Result<Vec<String>> {
        let resp = self.auth(self.http.get(self.models_url())).send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            anyhow::bail!("模型列表查询失败(HTTP {status}): {}", truncate(&text, 300));
        }
        let parsed: serde_json::Value = serde_json::from_str(&text)
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

    /// 连通性验证:发条最短请求确认端点/key/模型可用(配置向导用)
    pub async fn validate(&self) -> anyhow::Result<()> {
        // 用最小 token 的对话试;一些端点对空内容敏感,给个"hi"。
        // 有内容 / 无内容但请求成功都算可用(部分端点会返回空回复),所以这里只看请求本身成不成
        let messages = vec![ChatMessage::user("hi")];
        self.chat(&messages, None).await?;
        Ok(())
    }

    /// 鉴权 + 稳定会话头:唯一注头点。list_models/chat/chat_stream/validate(经 chat)
    /// 全经此发出，改一处即全生效；禁止裸 `self.http.post()` 绕过。
    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let req = match &self.api_key {
            Some(k) => req.bearer_auth(k),
            None => req,
        };
        match &self.session_header {
            Some(v) if !v.is_empty() => match reqwest::header::HeaderValue::from_str(v) {
                Ok(hv) => req.header(SESSION_HEADER, hv),
                Err(_) => {
                    eprintln!("⚠ 会话头值非法，已跳过（不影响本次请求）");
                    req
                }
            },
            _ => req,
        }
    }

    /// 组装请求体:流式与非流式只差 `stream` / `stream_options`,其余字段共用一份,
    /// 免得以后加 temperature / max_tokens 时漏改一边。
    fn build_body(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolDef]>,
        stream: bool,
    ) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
        let mut body = serde_json::Map::new();
        body.insert("model".into(), json!(self.model));
        body.insert("messages".into(), serde_json::to_value(messages)?);
        body.insert("user".into(), json!(REQ_USER));
        body.insert("stream".into(), json!(stream));
        if stream {
            // 让 OpenAI 兼容端点把真实用量放在流末尾的那条 usage 事件里
            body.insert("stream_options".into(), json!({"include_usage": true}));
        }
        if let Some(tools) = tools {
            body.insert("tools".into(), serde_json::to_value(tools)?);
        }
        Ok(body)
    }

    /// 发起一次非流式对话;`tools` 为 None 时不带工具声明
    pub async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolDef]>,
    ) -> anyhow::Result<AssistantReply> {
        let body = self.build_body(messages, tools, false)?;
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
        parse_chat_response(&text)
    }

    /// 流式对话:SSE 增量经 on_event 回调逐条送出,结束返回累积的完整回复
    /// (兼容 OpenAI 流式 data: 协议与 [DONE])
    pub async fn chat_stream<F>(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolDef]>,
        mut on_event: F,
    ) -> anyhow::Result<AssistantReply>
    where
        F: FnMut(StreamEvent),
    {
        let body = self.build_body(messages, tools, true)?;

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
        let mut acc = ToolCallAccumulator::default();
        let mut sse = SseParser::new();
        let mut stream = resp.bytes_stream();

        'stream: while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            for line in sse.push(&chunk) {
                if apply_stream_line(
                    &line,
                    &mut content,
                    &mut reasoning,
                    &mut usage,
                    &mut acc,
                    &mut on_event,
                )? {
                    break 'stream; // [DONE]
                }
            }
        }
        // 流结束:冲刷残余(个别服务端不补最后的空行)
        for line in sse.finish() {
            if apply_stream_line(
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

/// 处理一条 SSE `data:` 负载(一条 delta 或多条粘在同一事件里)。
/// 返回 true 表示遇到 [DONE]。
fn apply_stream_line(
    line: &str,
    content: &mut String,
    reasoning: &mut String,
    usage: &mut Usage,
    acc: &mut ToolCallAccumulator,
    on_event: &mut dyn FnMut(StreamEvent),
) -> Result<bool, anyhow::Error> {
    if line == "[DONE]" {
        return Ok(true);
    }
    let parsed: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return Ok(false), // 跳过无法解析的行,不中断流
    };
    // usage chunk(OpenAI 流式协议:include_usage 时流末尾发一条无 choices 的 usage)
    if parsed.get("usage").map(|u| !u.is_null()).unwrap_or(false) {
        let u = extract_usage(&parsed);
        if !u.is_empty() {
            *usage = u;
        }
    }
    let Some(choices) = parsed.get("choices").and_then(|c| c.as_array()) else {
        return Ok(false);
    };
    for choice in choices {
        let delta = choice.get("delta");
        if let Some(c) = delta
            .and_then(|d| d.get("content"))
            .and_then(|v| v.as_str())
        {
            if !c.is_empty() {
                content.push_str(c);
                on_event(StreamEvent::TextDelta(c.to_string()));
            }
        }
        if let Some(r) = delta
            .and_then(|d| d.get("reasoning_content"))
            .and_then(|v| v.as_str())
        {
            if !r.is_empty() {
                reasoning.push_str(r);
                on_event(StreamEvent::ReasoningDelta(r.to_string()));
            }
        }
        if let Some(calls) = delta
            .and_then(|d| d.get("tool_calls"))
            .and_then(|v| v.as_array())
        {
            for tc in calls {
                acc.merge_value(tc);
            }
        }
        if let Some(fr) = choice.get("finish_reason").and_then(|v| v.as_str()) {
            if fr == "length" {
                anyhow::bail!("模型输出超过上下文长度被截断(length)");
            }
        }
    }
    Ok(false)
}

/// `LlmClient` 薄转发:方法体调同名 inherent 方法(方法调用优先 inherent,无递归)。
/// chat 协议行为与抽取前逐行一致。
impl LlmClient for OpenAiClient {
    fn model(&self) -> &str {
        OpenAiClient::model(self)
    }

    fn reconfigure(&mut self, cfg: &Resolved) {
        OpenAiClient::reconfigure(self, cfg)
    }

    fn set_session_header(&mut self, enabled: bool, session_id: &str) {
        OpenAiClient::set_session_header(self, enabled, session_id)
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
        Box::pin(OpenAiClient::chat(self, messages, tools))
    }

    fn chat_stream<'a>(
        &'a self,
        messages: &'a [ChatMessage],
        tools: Option<&'a [ToolDef]>,
        on_event: &'a mut (dyn FnMut(StreamEvent) + Send),
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<AssistantReply>> + Send + 'a>,
    > {
        // `&mut dyn FnMut` 自身实现 `FnMut`,可直接填 inherent 的泛型 `F`
        Box::pin(OpenAiClient::chat_stream(self, messages, tools, on_event))
    }

    fn validate<'a>(
        &'a self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(OpenAiClient::validate(self))
    }

    fn list_models<'a>(
        &'a self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<Vec<String>>> + Send + 'a>>
    {
        Box::pin(OpenAiClient::list_models(self))
    }
}

/// 向导用:拿临时端点/key 探测模型列表,不碰全局状态。
/// 开关开时按开关带一次性探针头(验证阶段尚无正式 Session，仅走通缓存路由)。
/// proxy = 待存代理快照（验证阶段的模型列表查询同样走待存代理）。
pub async fn probe_models(
    base_url: &str,
    api_key: Option<&str>,
    session_header: Option<&str>,
    proxy: &crate::config::EffectiveProxy,
) -> anyhow::Result<Vec<String>> {
    let cfg = crate::config::Resolved {
        model: "probe".into(),
        base_url: base_url.to_string(),
        api_key: api_key.map(|s| s.to_string()),
        provider_name: "probe".into(),
        context_window: None,
        // GET /models 与协议无关,随便填 Chat
        protocol: crate::config::ProtocolKind::Chat,
        session_header_enabled: session_header.is_some(),
        retry: crate::config::RetryConfig::disabled(),
        proxy: proxy.clone(),
    };
    let mut client = OpenAiClient::new(&cfg)?;
    if let Some(v) = session_header {
        client.set_session_header(true, v);
    }
    client.list_models().await
}

/// 向导用:按给定配置发一条最小请求,验证连通/鉴权/模型可用。
/// 按 `probe.protocol` 自动选 chat / Responses 客户端。
/// 开关开但尚无真实会话 ID 时，用一次性探针值发头(与正式对话同路由)。
pub async fn probe_chat(probe: &crate::config::Resolved) -> anyhow::Result<()> {
    let mut client = crate::llm::client::build_llm_client(probe)?;
    if probe.session_header_enabled {
        client.set_session_header(true, &probe_session_value());
    }
    client.validate().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_serialize_omits_empty() {
        let m = ChatMessage::user("你好");
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["role"], "user");
        assert_eq!(v["content"], "你好");
        assert!(v.get("tool_calls").is_none());
        assert!(v.get("tool_call_id").is_none());
    }

    #[test]
    fn message_with_tool_call_serialize() {
        let m = ChatMessage::assistant_with_tool_calls(
            None,
            vec![ToolCall {
                id: "call_1".into(),
                call_type: "function".into(),
                function: crate::llm::types::FunctionCall {
                    name: "read_file".into(),
                    arguments: r#"{"path":"/tmp/a"}"#.into(),
                },
            }],
        );
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["role"], "assistant");
        assert!(v.get("content").is_none());
        assert_eq!(v["tool_calls"][0]["function"]["name"], "read_file");
    }

    #[test]
    fn sse_parse_basic() {
        let mut p = SseParser::new();
        let payloads = p.push(b"data: {\"a\":1}\n\ndata: {\"b\":2}\n\ndata: [DONE]\n\n");
        assert_eq!(payloads, vec![r#"{"a":1}"#, r#"{"b":2}"#, "[DONE]"]);
        assert!(p.finish().is_empty());
    }

    #[test]
    fn sse_parser_keeps_event_split_across_chunks() {
        // 回归:一条 data 行被拆在两个 HTTP chunk 里,旧实现会丢掉后半段
        let mut p = SseParser::new();
        assert!(p.push(b"data: {\"a\"").is_empty()); // 半条事件
        let payloads = p.push(b":1}\n\ndata: [DONE]\n\n");
        assert_eq!(payloads, vec![r#"{"a":1}"#, "[DONE]"]);
    }

    #[test]
    fn sse_parser_handles_crlf_and_missing_trailing_blank() {
        let mut p = SseParser::new();
        // \r\n\r\n 分隔
        let payloads = p.push(b"data: {\"x\":1}\r\n\r\ndata: {\"y\":2}\r\n\r\n");
        assert_eq!(payloads, vec![r#"{"x":1}"#, r#"{"y":2}"#]);
        // 最后一条事件没有收尾空行 → finish() 补出
        let mut p2 = SseParser::new();
        p2.push(b"data: {\"z\":3}\n\n");
        let tail = p2.push(b"data: {\"w\":4}");
        assert!(tail.is_empty());
        assert_eq!(p2.finish(), vec![r#"{"w":4}"#]);
    }

    #[test]
    fn tool_call_accumulator_merges_deltas() {
        // 标准分片:arguments 分多个字符串增量到达
        let mut acc = ToolCallAccumulator::default();
        acc.merge_value(&json!({
            "index": 0, "id": "call_1", "type": "function",
            "function": {"name": "read_file", "arguments": "{\"path\":"}
        }));
        acc.merge_value(&json!({
            "index": 0, "function": {"arguments": "\"/tmp/a\"}"}
        }));
        let calls = acc.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].function.name, "read_file");
        assert_eq!(calls[0].function.arguments, r#"{"path":"/tmp/a"}"#);
    }

    #[test]
    fn tool_call_object_arguments_tolerated() {
        // 部分端点把 arguments 直接给 JSON 对象 → 序列化为字符串,工具不再拿不到参数
        let mut acc = ToolCallAccumulator::default();
        acc.merge_value(&json!({
            "index": 0, "id": "c2",
            "function": {"name": "run_shell_command", "arguments": {"command": "ls -la"}}
        }));
        let calls = acc.finish();
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0]
                .function
                .arguments
                .contains("\"command\":\"ls -la\""),
            "got: {}",
            calls[0].function.arguments
        );
    }

    #[test]
    fn stream_delta_without_index_and_full_object() {
        // 部分端点(如 llama.cpp 风格)一次性给出完整参数对象、且不带 index
        let mut acc = ToolCallAccumulator::default();
        acc.merge_value(&json!({
            "function": {"name": "memory_write",
                         "arguments": {"name": "偏好", "content": "喜欢中文"}}
        }));
        let calls = acc.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_memory_write"); // 缺省 id
        assert!(calls[0].function.arguments.contains("\"name\":\"偏好\""));
    }

    #[test]
    fn non_stream_tolerates_object_args_and_content_array() {
        let text = r#"{"choices":[{"message":{
            "role":"assistant",
            "content":[{"type":"text","text":"好的,马上办"}],
            "tool_calls":[{"id":"c1","type":"function","function":{
                "name":"write_file","arguments":{"path":"a.txt","content":"hi"}
            }}]
        }}]}"#;
        let reply = parse_chat_response(text).unwrap();
        assert_eq!(reply.content.as_deref(), Some("好的,马上办"));
        assert_eq!(reply.tool_calls.len(), 1);
        assert!(reply.tool_calls[0]
            .function
            .arguments
            .contains("\"path\":\"a.txt\""));
        assert_eq!(reply.tool_calls[0].id, "c1");
    }

    #[test]
    fn non_stream_empty_arguments_ok() {
        // 模型输出空 arguments(字符串)时不再让整个回复解析失败
        let text = r#"{"choices":[{"message":{
            "role":"assistant",
            "tool_calls":[{"id":"c2","type":"function","function":{
                "name":"list_directory","arguments":""
            }}]
        }}]}"#;
        let reply = parse_chat_response(text).unwrap();
        assert_eq!(reply.tool_calls.len(), 1);
        assert_eq!(reply.tool_calls[0].function.arguments, "");
    }

    #[test]
    fn non_stream_extracts_usage() {
        let text = r#"{"choices":[{"message":{"role":"assistant","content":"hi"}}],
            "usage":{"prompt_tokens":12,"completion_tokens":5,"total_tokens":17}}"#;
        let reply = parse_chat_response(text).unwrap();
        assert_eq!(reply.content.as_deref(), Some("hi"));
        assert_eq!(reply.usage.prompt_tokens, 12);
        assert_eq!(reply.usage.completion_tokens, 5);
        assert_eq!(reply.usage.total(), 17);
    }

    #[test]
    fn missing_usage_is_zero() {
        // 端点不返回 usage → 不 panic,用量记 0
        let text = r#"{"choices":[{"message":{"role":"assistant","content":"x"}}]}"#;
        let reply = parse_chat_response(text).unwrap();
        assert!(reply.usage.is_empty());
    }

    #[test]
    fn stream_usage_chunk_parsed() {
        // OpenAI 流式协议:include_usage 时流末尾发一条无 choices 的 usage 事件
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut usage = Usage::default();
        let mut acc = ToolCallAccumulator::default();
        let mut events: Vec<StreamEvent> = Vec::new();

        let done = apply_stream_line(
            r#"{"choices":[{"delta":{"content":"你好"},"finish_reason":null}]}"#,
            &mut content,
            &mut reasoning,
            &mut usage,
            &mut acc,
            &mut |e| events.push(e),
        )
        .unwrap();
        assert!(!done);
        assert_eq!(content, "你好");

        let done = apply_stream_line(
            r#"{"choices":[],"usage":{"prompt_tokens":30,"completion_tokens":2,"total_tokens":32}}"#,
            &mut content,
            &mut reasoning,
            &mut usage,
            &mut acc,
            &mut |e| events.push(e),
        )
        .unwrap();
        assert!(!done);
        assert_eq!(content, "你好"); // usage chunk 不产生文本
        assert_eq!(usage.prompt_tokens, 30);
        assert_eq!(usage.completion_tokens, 2);
        assert_eq!(usage.total(), 32);
        // usage chunk 不产生任何增量事件
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], StreamEvent::TextDelta(_)));
    }

    fn test_cfg(enabled: bool) -> Resolved {
        Resolved {
            model: "m".into(),
            base_url: "http://localhost:11434/v1".into(),
            api_key: None,
            provider_name: "t".into(),
            context_window: None,
            protocol: crate::config::ProtocolKind::Chat,
            session_header_enabled: enabled,
            retry: crate::config::RetryConfig::disabled(),
            proxy: crate::config::EffectiveProxy::Direct,
        }
    }

    fn built_headers(c: &OpenAiClient) -> reqwest::header::HeaderMap {
        c.auth(c.http.get("http://localhost:11434/v1/models"))
            .build()
            .unwrap()
            .headers()
            .clone()
    }

    #[test]
    fn auth_session_header_off_placeholder_and_on() {
        // 关 → 无头
        let c = OpenAiClient::new(&test_cfg(false)).unwrap();
        assert!(built_headers(&c).get(SESSION_HEADER).is_none());
        // 开但占位(空串，值未到) → 不发
        let c = OpenAiClient::new(&test_cfg(true)).unwrap();
        assert!(built_headers(&c).get(SESSION_HEADER).is_none());
        // 开 + 回填 → 发头且值相等
        let mut c = OpenAiClient::new(&test_cfg(true)).unwrap();
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
        let mut c2 = OpenAiClient::new(&test_cfg(true)).unwrap();
        c2.set_session_header(true, "bad\nvalue");
        assert!(built_headers(&c2).get(SESSION_HEADER).is_none());
        // 探针值形态：wizard- 前缀 + hex
        let p = probe_session_value();
        assert!(p.starts_with("wizard-"), "got: {p}");
    }
}
