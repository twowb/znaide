use crate::config::{ProtocolKind, Resolved};
use crate::llm::openai::{AssistantReply, OpenAiClient, StreamEvent};
use crate::llm::responses::ResponsesClient;
use crate::llm::types::{ChatMessage, ToolDef};
use std::future::Future;
use std::pin::Pin;

/// LLM 客户端抽象:调用方(Session/app/cli)只认它,不感知 chat / response 协议细节。
/// 手写 `Pin<Box<dyn Future>>` 而不是引 `async-trait`,workspace 零新依赖。
/// `chat_stream` 的回调取 `&mut dyn FnMut`,调用点 `&mut |evt| {...}` 照常传入。
pub trait LlmClient: Send + Sync {
    fn model(&self) -> &str;

    fn reconfigure(&mut self, cfg: &Resolved);

    /// 推送会话头开关 + 值(Session 建好/恢复/重配后调，值 = session_id)。
    /// 默认空实现：第三方 LlmClient 实现无需改动；双内置客户端各自 override。
    fn set_session_header(&mut self, _enabled: bool, _session_id: &str) {}

    /// 当前生效代理快照（默认直连；双内置客户端各自 override 返回真实值）。
    /// Session 建会话时用它初始化工具链路快照，与 llm 实际出口保持一致。
    fn effective_proxy(&self) -> crate::config::EffectiveProxy {
        crate::config::EffectiveProxy::Direct
    }

    fn chat<'a>(
        &'a self,
        messages: &'a [ChatMessage],
        tools: Option<&'a [ToolDef]>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<AssistantReply>> + Send + 'a>>;

    fn chat_stream<'a>(
        &'a self,
        messages: &'a [ChatMessage],
        tools: Option<&'a [ToolDef]>,
        on_event: &'a mut (dyn FnMut(StreamEvent) + Send),
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<AssistantReply>> + Send + 'a>>;

    fn validate<'a>(&'a self) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

    fn list_models<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Vec<String>>> + Send + 'a>>;
}

/// 老函数变 wrapper：Auto 语义（跟随环境）。非法 env 吞错直连（零回归），
/// 显式地址的 Err 只在 with_proxy 版本透出。
pub fn new_http_client() -> anyhow::Result<reqwest::Client> {
    let env = crate::update::env_proxy_url();
    match new_http_client_with_proxy(env.as_deref()) {
        Ok(c) => Ok(c),
        Err(_) => new_http_client_with_proxy(None),
    }
}

/// 按生效代理构造（超时 600s + connect 10s 不变）。
/// None = 直连；Some(url) = 走该出口（含 NO_PROXY 豁免）。
/// 显式地址非法 → Err（调用方警告 + 降级直连）；env 缺失即直连。
pub fn new_http_client_with_proxy(proxy_url: Option<&str>) -> anyhow::Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        // 连接阶段单独限时(DNS/拒连/半开黑洞 10s 内报错),
        // 而不是干等 600s 总超时才失败
        .connect_timeout(std::time::Duration::from_secs(10));
    // 代理走生效地址(含 NO_PROXY 豁免);None 即直连;本地 ollama 默认不受影响
    if let Some(u) = proxy_url.map(str::trim).filter(|u| !u.is_empty()) {
        let p = crate::update::proxy_with_url(u)?;
        builder = builder.proxy(p);
    }
    Ok(builder.build()?)
}

/// 共享:HTTP 非 2xx 统一文案,两客户端保证一致。
pub fn http_error(status: reqwest::StatusCode, text: &str) -> anyhow::Error {
    anyhow::anyhow!("模型端点返回 {status}: {}", truncate(text, 500))
}

/// 工厂:调用方唯一入口,按 `Resolved.protocol` 建对应客户端
pub fn build_llm_client(cfg: &Resolved) -> anyhow::Result<Box<dyn LlmClient>> {
    match cfg.protocol {
        ProtocolKind::Chat => Ok(Box::new(OpenAiClient::new(cfg)?)),
        ProtocolKind::Response => Ok(Box::new(ResponsesClient::new(cfg)?)),
    }
}

pub fn truncate(s: &str, max: usize) -> String {
    crate::util::truncate_chars(s, max, "…(截断)")
}
