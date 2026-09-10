use crate::config::Resolved;
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
}

/// 请求 user 标识:chat 请求顶层 user 字段(遥测/实例区分用)
const REQ_USER: &str = "6ba93f8a8d2e";

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
    let n = |k: &str| u.and_then(|v| v.get(k)).and_then(|v| v.as_u64()).unwrap_or(0);
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
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(600))
            // 连接阶段单独限时(DNS/拒连/半开黑洞 10s 内报错),
            // 而不是干等 600s 总超时才失败
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()?;
        Ok(Self {
            http,
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            api_key: cfg.api_key.clone(),
            model: cfg.model.clone(),
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// 运行时换端点/key/模型(不动 http client,连接池留着复用)
    pub fn reconfigure(&mut self, cfg: &Resolved) {
        self.base_url = cfg.base_url.trim_end_matches('/').to_string();
        self.model = cfg.model.clone();
        self.api_key = cfg.api_key.clone();
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

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(k) => req.bearer_auth(k),
            None => req,
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
        let resp = self.auth(self.http.post(self.url())).json(&body).send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            anyhow::bail!("模型端点返回 {status}: {}", truncate(&text, 500));
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
            anyhow::bail!("模型端点返回 {status}: {}", truncate(&text, 500));
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
                if apply_stream_line(&line, &mut content, &mut reasoning, &mut usage, &mut acc, &mut on_event)? {
                    break 'stream; // [DONE]
                }
            }
        }
        // 流结束:冲刷残余(个别服务端不补最后的空行)
        for line in sse.finish() {
            if apply_stream_line(&line, &mut content, &mut reasoning, &mut usage, &mut acc, &mut on_event)? {
                break;
            }
        }

        Ok(AssistantReply {
            content: if content.is_empty() { None } else { Some(content) },
            reasoning_content: if reasoning.is_empty() { None } else { Some(reasoning) },
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

/// SSE 增量解析器。关键点:一条事件(`data: ...` 行)可能被拆在多个 HTTP chunk 里,
/// 必须跨 chunk 缓冲,否则后半段(没有 `data:` 前缀)会被丢掉 → 工具 arguments 被截断。
struct SseParser {
    buf: String,
}

impl SseParser {
    fn new() -> Self {
        Self { buf: String::new() }
    }

    /// 送入一块网络数据,返回其中完整事件的 `data:` 负载(已去前缀)。
    /// 事件以空行分隔;兼容 \n 与 \r\n。未闭合的部分留到下次。
    fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        // 先把换行统一成 \n(兼容 \r\n / \r),跨 chunk 的 \r\n 也能被下次拼上
        let text = String::from_utf8_lossy(chunk).replace("\r\n", "\n").replace('\r', "\n");
        self.buf.push_str(&text);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.find("\n\n") {
            let event = self.buf[..pos].to_string();
            self.buf.drain(..pos + 2);
            for line in event.lines() {
                if let Some(rest) = line.strip_prefix("data:") {
                    out.push(rest.trim().to_string());
                }
            }
        }
        out
    }

    /// 流结束时的残余数据(服务端可能不补最后一个空行)
    fn finish(&mut self) -> Vec<String> {
        if self.buf.trim().is_empty() {
            return Vec::new();
        }
        let event = std::mem::take(&mut self.buf);
        let mut out = Vec::new();
        for line in event.lines() {
            if let Some(rest) = line.strip_prefix("data:") {
                out.push(rest.trim().to_string());
            }
        }
        out
    }
}

/// 向导用:拿临时端点/key 探测模型列表,不碰全局状态
pub async fn probe_models(base_url: &str, api_key: Option<&str>) -> anyhow::Result<Vec<String>> {
    let cfg = crate::config::Resolved {
        model: "probe".into(),
        base_url: base_url.to_string(),
        api_key: api_key.map(|s| s.to_string()),
        provider_name: "probe".into(),
        context_window: None,
    };
    let client = OpenAiClient::new(&cfg)?;
    client.list_models().await
}

/// 向导用:按给定配置发一条最小请求,验证连通/鉴权/模型可用
pub async fn probe_chat(probe: &crate::config::Resolved) -> anyhow::Result<()> {
    let client = OpenAiClient::new(probe)?;
    client.validate().await
}

fn truncate(s: &str, max: usize) -> String {
    crate::util::truncate_chars(s, max, "…(截断)")
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
            calls[0].function.arguments.contains("\"command\":\"ls -la\""),
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
        assert!(reply.tool_calls[0].function.arguments.contains("\"path\":\"a.txt\""));
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
}
