use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 数据目录:默认 ~/.znaide,ZNAIDE_DATA_DIR 可覆盖(测试/多实例用)
pub fn data_dir() -> PathBuf {
    if let Ok(override_dir) = std::env::var("ZNAIDE_DATA_DIR") {
        return PathBuf::from(override_dir);
    }
    dirs::home_dir()
        .map(|h| h.join(".znaide"))
        .unwrap_or_else(|| PathBuf::from(".znaide"))
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 默认模型,如 "qwen3:8b" / "deepseek-chat"
    pub model: Option<String>,
    /// OpenAI 兼容端点,如 http://localhost:11434/v1
    pub base_url: Option<String>,
    /// API key(本地端点可留空)
    pub api_key: Option<String>,
    /// 当前 provider(见 providers 表)
    pub provider: Option<String>,
    pub providers: std::collections::HashMap<String, ProviderDef>,
    /// 上下文窗口(token),按 provider 各配各的;不设则按模型名查内置表,
    /// **查不到就是"未知"**(不再兜底 32k,见 `effective_context_window`)。
    /// ollama 要和运行时的 num_ctx 对上,否则占用条不准。
    pub context_window: Option<usize>,
    /// 单条消息最多允许的模型往返轮数(一轮可含多次工具调用)。不设 = 200;
    /// 0 = 不限(只靠"重复调用同一工具"刹车)。命令行 `--max-turns` 优先于这里。
    pub max_turns: Option<usize>,
    /// 全局人格(见 persona 模块);空 = 不注入。切换会写回这里持久生效。
    pub persona: Option<String>,
    /// 协议类型手动覆盖(chat/response),优先于 provider 条目;面板保存时清空。
    pub protocol: Option<ProtocolKind>,
    /// 弱网增强重试(默认关闭)。开启后单次模型调用遇空回复/可重试错误时
    /// 按次数重试(统一退避)，耗尽才算彻底失败。
    #[serde(default)]
    pub retry: RetryConfig,
    /// 全局网络代理(默认 Auto = 跟随环境，零回归)。与 retry 同口径的顶层字段，
    /// 不跟 provider 走；落定值随 Resolved 走全链路。
    #[serde(default)]
    pub proxy: ProxyConfig,
    /// 配置格式版本(内部键):保存时缺失自动补齐,供将来迁移判断。
    #[serde(default)]
    pub build_tag: Option<String>,
}

/// 弱网重试配置(顶层 `retry`)。默认关闭，零行为变化。
/// `max_retries` = 首次失败后的追加次数(3 即最多调 1+3=4 次)，落定时夹取 0..=8。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RetryConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_retry_times")]
    pub max_retries: usize,
}

fn default_retry_times() -> usize {
    3
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_retries: default_retry_times(),
        }
    }
}

impl RetryConfig {
    /// 允许的档位：关闭/3/5/8/手动(0..=8)。非法值返回 Err 提示。
    pub fn from_option(max_retries: Option<usize>) -> Self {
        match max_retries {
            None => Self {
                enabled: false,
                max_retries: default_retry_times(),
            },
            Some(n) => Self {
                enabled: true,
                max_retries: n.min(MAX_RETRIES),
            },
        }
    }

    pub fn enabled_with(n: usize) -> Self {
        Self {
            enabled: true,
            max_retries: n.min(MAX_RETRIES),
        }
    }

    pub fn disabled() -> Self {
        Self::default()
    }

    /// 生效次数(关 = 0；开 = 夹取后的值)
    pub fn effective_times(&self) -> usize {
        if self.enabled {
            self.max_retries.min(MAX_RETRIES)
        } else {
            0
        }
    }
}

/// 重试次数上限(手动输入也夹取到此值)
pub const MAX_RETRIES: usize = 8;
/// 统一退避基准(毫秒)：delay = BASE * 2^attempt + 0~200ms 抖动
pub const RETRY_BACKOFF_BASE_MS: u64 = 800;

/// 当前配置格式版本(写入 config.json 的 build_tag)。
/// v2:顶层 model/base_url/api_key/context_window 不再由面板写入,改为搬进对应
/// provider 条目(顶层只作手动临时覆盖),因此换一次标记触发一次性自愈迁移。
const CONFIG_TAG: &str = "c6ee35b45916";

/// 协议类型:按 provider 条目存储,与 `context_window` 同口径。
/// 缺省(chat) = `POST /chat/completions`(现状);`response` = `POST /responses`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProtocolKind {
    #[default]
    Chat,
    Response,
}

impl ProtocolKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProtocolKind::Chat => "chat",
            ProtocolKind::Response => "response",
        }
    }

    /// 大小写不敏感;`responses`(复数拼写)也认。非法值 → None。
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "chat" => Some(ProtocolKind::Chat),
            "response" | "responses" => Some(ProtocolKind::Response),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            ProtocolKind::Chat => "chat（对话补全 /chat/completions）",
            ProtocolKind::Response => "response（Responses /responses）",
        }
    }
}

/// provider 预设:端点 + 默认模型 + key(环境变量名或明文)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderDef {
    pub base_url: Option<String>,
    /// 默认模型(没显式指定 model 时用它)
    pub model: Option<String>,
    /// 从该环境变量读 key,如 "DASHSCOPE_API_KEY"
    pub api_key_env: Option<String>,
    /// 明文 key(优先于 api_key_env)
    pub api_key: Option<String>,
    /// 该服务商对应模型的上下文窗口(可选)。多 provider 各配各的,
    /// 避免在 40k 的 ollama 与 1M 的云端模型之间切换时占用条算错。
    pub context_window: Option<usize>,
    /// 该 provider 的协议:None = chat(老配置/未填的默认值)
    pub protocol: Option<ProtocolKind>,
    /// 是否为该家启用 x-opencode-session 会话头(默认 false，老配置缺键自动关)。
    /// 开启后同一会话内所有模型请求携带同一稳定值(会话 ID)，提升服务端 KV 缓存命中率。
    #[serde(default)]
    pub session_header: bool,
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub model: String,
    pub base_url: String,
    pub api_key: Option<String>,
    /// 当前 provider 名(展示用)
    pub provider_name: String,
    /// 上下文窗口(None = 查内置表)
    pub context_window: Option<usize>,
    /// 确定值(非 Option):resolve 时已落定,调用方可直接 match
    pub protocol: ProtocolKind,
    /// 本次运行是否发会话头(开关，具体头值由 Session 侧供给会话 ID)
    pub session_header_enabled: bool,
    /// 弱网重试策略(resolve 时已落定，含 CLI/ENV 覆盖)
    pub retry: RetryConfig,
    /// 本次运行的生效代理快照(resolve 时已落定文件值，CLI/ENV 覆盖由宿主覆写)
    pub proxy: EffectiveProxy,
}

/// 全局网络代理三档(顶层 `proxy` 原文)。默认 Auto = 跟随环境，零回归。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct ProxyConfig {
    /// auto（默认）/ off / manual（大小写不敏感；非法 → auto 并警告）
    pub mode: ProxyMode,
    /// manual 档的地址；auto/off 下忽略（保存时清空，保持文件干净）
    pub url: Option<String>,
}

/// 手写 Deserialize：`mode` 先读 String，`parse` 失败 → Auto + 警告，
/// 避免手写 `"mode":"foo"` 炸掉整份 config 反序列化。
impl<'de> serde::Deserialize<'de> for ProxyConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        struct ProxyConfigRaw {
            #[serde(default)]
            mode: Option<String>,
            #[serde(default)]
            url: Option<String>,
        }
        let raw = ProxyConfigRaw::deserialize(deserializer)?;
        let mode = match raw.mode.as_deref() {
            None | Some("") => ProxyMode::Auto,
            Some(s) => match ProxyMode::parse(s) {
                Some(m) => m,
                None => {
                    eprintln!("⚠ 配置文件 proxy.mode={s:?} 无法识别(应为 auto/off/manual)，已按跟随环境处理");
                    ProxyMode::Auto
                }
            },
        };
        Ok(ProxyConfig { mode, url: raw.url })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyMode {
    #[default]
    Auto,
    Off,
    Manual,
}

impl ProxyMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "auto" | "env" | "follow" => Some(ProxyMode::Auto),
            "off" | "none" | "direct" | "直连" => Some(ProxyMode::Off),
            "manual" | "custom" | "手动" => Some(ProxyMode::Manual),
            _ => None,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            ProxyMode::Auto => "auto",
            ProxyMode::Off => "off",
            ProxyMode::Manual => "manual",
        }
    }
}

/// 本次运行的生效代理（快照，随 Resolved 走全链路）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum EffectiveProxy {
    /// 直连（Off 强制 / 各层都没配出来）
    #[default]
    Direct,
    /// 走该地址（含 NO_PROXY 豁免）
    Via(String),
}

impl EffectiveProxy {
    pub fn url(&self) -> Option<&str> {
        match self {
            EffectiveProxy::Direct => None,
            EffectiveProxy::Via(u) => Some(u),
        }
    }
    /// 菜单/日志展示（密码脱敏：只露 scheme://host:port）
    pub fn display(&self) -> String {
        match self {
            EffectiveProxy::Direct => "直连".to_string(),
            EffectiveProxy::Via(u) => mask_proxy_url(u),
        }
    }
}

/// 代理 URL 脱敏：去掉 `userinfo@` 段，scheme/host/port 原样。
pub fn mask_proxy_url(u: &str) -> String {
    // scheme://[userinfo@]rest → scheme://rest
    if let Some(scheme_end) = u.find("://") {
        let (scheme, rest) = u.split_at(scheme_end + 3);
        if let Some(at) = rest.find('@') {
            // '@' 之后才是 host 部分（userinfo 里不会有 '/'，先确认 '@' 在首个 '/' 之前）
            let before_slash = rest.find('/').unwrap_or(rest.len());
            if at < before_slash {
                return format!("{scheme}{}", &rest[at + 1..]);
            }
        }
        return u.to_string();
    }
    u.to_string()
}

/// 代理地址校验：scheme ∈ {http, https, socks5, socks5h} 且有 host。
/// 返回归一后的 url（去首尾空白与尾斜杠，scheme 小写）。
pub fn normalize_proxy_url(v: &str) -> anyhow::Result<String> {
    let t = v.trim();
    if t.is_empty() {
        anyhow::bail!("代理地址要形如 http(s)://host:port 或 socks5(h)://host:port，当前是空串");
    }
    let scheme_end = t.find("://").ok_or_else(|| {
        anyhow::anyhow!("代理地址要形如 http(s)://host:port 或 socks5(h)://host:port，当前是 {v:?}")
    })?;
    let (scheme_raw, rest) = t.split_at(scheme_end + 3);
    let scheme = scheme_raw[..scheme_raw.len() - 3].to_lowercase();
    if !matches!(scheme.as_str(), "http" | "https" | "socks5" | "socks5h") {
        anyhow::bail!("代理地址要形如 http(s)://host:port 或 socks5(h)://host:port，当前是 {v:?}");
    }
    // rest = [userinfo@]host[:port][/...]，host 不能为空
    let host_part = rest.split('/').next().unwrap_or("");
    let host_part = match host_part.rfind('@') {
        Some(i) => &host_part[i + 1..],
        None => host_part,
    };
    if host_part.is_empty() {
        anyhow::bail!("代理地址要形如 http(s)://host:port 或 socks5(h)://host:port，当前是 {v:?}");
    }
    let normalized = format!("{scheme}://{rest}");
    Ok(normalized.trim_end_matches('/').to_string())
}

/// TUI 输入态轻量预检（与 confirm_typed 同口径，不抛 anyhow）。
pub fn valid_proxy_url(v: &str) -> bool {
    normalize_proxy_url(v).is_ok()
}

/// 模型窗口内置表(没配 context_window 时按名字匹配)。
/// 数据 2026-09 从各家官方页 + Litellm 扒的,迭代很快,过时了就改;
/// 本地模型窗口看 ollama 的 num_ctx,对不上就在 config.json 写死。
/// **查不到一律返回 None(未知)**:以前这里回 32k,会把百万级模型算成"快满了",
/// 占用条虚高还会劝人做没必要的 /compact。
fn model_context_window(model: &str) -> Option<usize> {
    let m = model.to_lowercase();
    let has = |keys: &[&str]| keys.iter().any(|k| m.contains(k));
    // ---- 闭源/云端 ----
    let win: usize = if has(&["claude"]) {
        200_000 // opus/sonnet/haiku 4.x-5.x
    } else if has(&["gpt-5"]) {
        400_000 // gpt-5 / gpt-5-mini / gpt-5-nano
    } else if has(&["gpt-4.1"]) {
        1_048_576
    } else if has(&["o4-mini", "o3", "o1"]) {
        200_000
    } else if has(&["gpt-4o"]) {
        128_000
    } else if has(&["gemini"]) || has(&["qwen-plus"]) {
        1_000_000 // gemini-2.5 系(官方站限区未能复核);qwen-plus(百炼 Qwen3 系)
    } else if has(&["qwen3-max", "qwen3.8-max"]) {
        1_000_000 // 百炼 Qwen3 代 max 档:与表内同代的 qwen-plus 对齐(确切值请写进条目覆盖)
    } else if has(&["qwen-max"]) {
        32_768 // 阿里百炼 qwen-max 官方页
    } else if has(&["deepseek-v4", "deepseek-flash", "deepseek-pro"]) {
        1_048_576 // DeepSeek v4(flash/pro)官方页 1M
    } else if has(&["deepseek-chat", "deepseek-reasoner"]) {
        131_072
    } else if has(&["glm-5"]) {
        1_048_576 // 智谱 GLM-5.3-flash 官方页 1M
    } else if has(&["glm-4", "glm4"]) {
        131_072
    } else if has(&["kimi-k3", "k3"]) {
        1_000_000 // 月之暗面 Kimi K3
    } else if has(&["kimi-k2", "moonshot-v1"]) {
        262_144 // Kimi K2 系列(2.6/2.7 官方 256k)
    } else if has(&["kimi"]) {
        131_072
    }
    // ---- 本地/开源(ollama 常用)----
    else if has(&["qwen3:4b", "qwen3:30b", "qwen3:235b"]) {
        262_144
    } else if has(&["qwen3"]) {
        40_960 // ollama qwen3:8b/14b/32b 等默认 40k
    } else if has(&["qwen2.5-coder", "qwen2.5", "qwen-coder"]) {
        32_768
    } else if has(&["llama3.3", "llama3.2", "llama3.1"]) {
        131_072 // ollama llama3.x 默认 128k
    } else if has(&["llama3"]) {
        8_192
    } else {
        // 认不出来就认"不知道",不拿默认值假装知道
        return None;
    };
    Some(win)
}

fn env_first(names: &[&str]) -> Option<String> {
    names.iter().find_map(|n| std::env::var(n).ok())
}

/// 解析 ZNAIDE_SESSION_HEADER:1/true/yes/on → 开;0/false/no/off → 关(大小写不敏感);
/// 空/非法 → None(调用方忽略并提示)。
pub fn parse_session_header_env(s: &str) -> Option<bool> {
    match s.trim().to_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// 内置 provider 预设;config.providers 可覆盖同名项
pub fn builtin_providers() -> std::collections::HashMap<String, ProviderDef> {
    let mut m = std::collections::HashMap::new();
    m.insert(
        "ollama".into(),
        ProviderDef {
            base_url: Some("http://localhost:11434/v1".into()),
            model: Some("qwen3:8b".into()),
            ..Default::default()
        },
    );
    m.insert(
        "dashscope".into(),
        ProviderDef {
            base_url: Some("https://dashscope.aliyuncs.com/compatible-mode/v1".into()),
            model: Some("qwen-plus".into()),
            api_key_env: Some("DASHSCOPE_API_KEY".into()),
            ..Default::default()
        },
    );
    m.insert(
        "deepseek".into(),
        ProviderDef {
            base_url: Some("https://api.deepseek.com/v1".into()),
            model: Some("deepseek-chat".into()),
            api_key_env: Some("DEEPSEEK_API_KEY".into()),
            ..Default::default()
        },
    );
    m.insert(
        "openrouter".into(),
        ProviderDef {
            base_url: Some("https://openrouter.ai/api/v1".into()),
            model: Some("qwen/qwen3-8b".into()),
            api_key_env: Some("OPENROUTER_API_KEY".into()),
            ..Default::default()
        },
    );
    m.insert(
        "zai".into(),
        ProviderDef {
            base_url: Some("https://api.z.ai/api/v1".into()),
            model: Some("deepseek-ai/DeepSeek-R1-Distill-Qwen-32B".into()),
            api_key_env: Some("ZAI_API_KEY".into()),
            ..Default::default()
        },
    );
    m
}

impl Config {
    /// 合并后的 provider 表(内置 + 用户覆盖/新增)
    pub fn all_providers(&self) -> std::collections::HashMap<String, ProviderDef> {
        let mut m = builtin_providers();
        // 字段级合并:内置条目为底,用户条目只覆盖它写了值的字段。
        // (整条替换会让 `{"base_url": ...}` 顺手丢掉内置的 model/api_key_env)
        for (k, v) in &self.providers {
            match m.get_mut(k) {
                Some(base) => {
                    if v.base_url.is_some() {
                        base.base_url = v.base_url.clone();
                    }
                    if v.model.is_some() {
                        base.model = v.model.clone();
                    }
                    if v.api_key.is_some() {
                        base.api_key = v.api_key.clone();
                    }
                    if v.api_key_env.is_some() {
                        base.api_key_env = v.api_key_env.clone();
                    }
                    if v.context_window.is_some() {
                        base.context_window = v.context_window;
                    }
                    if v.protocol.is_some() {
                        base.protocol = v.protocol;
                    }
                    // 布尔只能"或":false 无法覆盖内置 true。内置预设全为 false，故无歧义；
                    // 将来若内置某家默认开，再改为 Option<bool>。
                    if v.session_header {
                        base.session_header = true;
                    }
                }
                None => {
                    m.insert(k.clone(), v.clone());
                }
            }
        }
        m
    }

    /// 读配置;文件不存在返回默认
    pub fn load() -> anyhow::Result<Self> {
        let path = config_path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)?;
        let cfg: Config = serde_json::from_str(&text)?;
        Ok(cfg)
    }

    /// 算最终运行参数,来源优先级:CLI > 环境变量 > config > 内置预设
    pub fn resolve(
        &self,
        cli_model: Option<String>,
        cli_base_url: Option<String>,
        cli_api_key: Option<String>,
        cli_provider: Option<String>,
        cli_protocol: Option<ProtocolKind>,
        cli_session_header: Option<bool>,
    ) -> anyhow::Result<Resolved> {
        let provider_name = cli_provider
            .or_else(|| env_first(&["ZNAIDE_PROVIDER"]))
            .or_else(|| self.provider.clone())
            .unwrap_or_else(|| "ollama".to_string());
        let providers = self.all_providers();
        let pdef = providers
            .get(&provider_name)
            .cloned()
            .unwrap_or_else(|| ProviderDef {
                base_url: Some(format!("https://{provider_name}/v1")),
                model: None,
                ..Default::default()
            });

        // base_url:CLI > env > 顶层 config > provider 预设
        let base_url = cli_base_url
            .or_else(|| env_first(&["ZNAIDE_BASE_URL", "OPENAI_BASE_URL"]))
            .or_else(|| self.base_url.clone())
            .or(pdef.base_url.clone())
            .unwrap_or_else(|| "http://localhost:11434/v1".to_string());

        // model:CLI > env > 顶层 config > 预设;再没有就报错
        let model = cli_model
            .or_else(|| env_first(&["ZNAIDE_MODEL", "OPENAI_MODEL"]))
            .or_else(|| self.model.clone())
            .or(pdef.model.clone())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "未配置模型:请用 --model 指定,或设置 ZNAIDE_MODEL / config.json 的 model / provider「{provider_name}」的 model"
                )
            })?;

        // key:CLI > env > 顶层 config(手动覆盖)> provider 明文 > provider 环境变量名。
        // 三个字段口径统一为"顶层覆盖预设",避免切 provider 时出现"新 key + 老端点"的错配。
        let api_key = cli_api_key
            .or_else(|| env_first(&["ZNAIDE_API_KEY", "OPENAI_API_KEY"]))
            .or_else(|| self.api_key.clone().filter(|k| !k.is_empty()))
            .or_else(|| pdef.api_key.clone().filter(|k| !k.is_empty()))
            .or_else(|| pdef.api_key_env.as_ref().and_then(|env| env_first(&[env])));

        // 协议:CLI > env(ZNAIDE_PROTOCOL) > 顶层 config(手动覆盖) > provider 条目 > 默认 Chat。
        // 非法 env 值忽略并提示,不 hard fail(避免换台机器因拼写起不来)。
        let env_protocol =
            env_first(&["ZNAIDE_PROTOCOL"]).and_then(|s| match ProtocolKind::parse(&s) {
                Some(p) => Some(p),
                None => {
                    eprintln!(
                        "⚠ 环境变量 ZNAIDE_PROTOCOL={s:?} 无法识别(应为 chat/response),已忽略"
                    );
                    None
                }
            });
        let protocol = cli_protocol
            .or(env_protocol)
            .or(self.protocol)
            .or(pdef.protocol)
            .unwrap_or_default();

        // 会话头开关:CLI > env(ZNAIDE_SESSION_HEADER) > provider 条目 > 默认关。
        // 非法 env 值忽略并提示,不 hard fail(与 ZNAIDE_PROTOCOL 同口径)。
        let env_session_header = env_first(&["ZNAIDE_SESSION_HEADER"]).and_then(|s| {
            match parse_session_header_env(&s) {
                Some(v) => Some(v),
                None => {
                    eprintln!("⚠ 环境变量 ZNAIDE_SESSION_HEADER={s:?} 无法识别(应为 1/true/yes/on 或 0/false/no/off),已忽略");
                    None
                }
            }
        });
        let session_header_enabled = cli_session_header
            .or(env_session_header)
            .unwrap_or(pdef.session_header);

        // 重试策略：resolve() 只落定 config 文件值；CLI/ENV 覆盖由 resolve_retry() 统一处理，
        // 调用方在 resolve() 之后用它覆写 resolved.retry（保持 resolve 签名不变，老调用方零改动）。
        let mut retry = self.retry;
        retry.max_retries = retry.max_retries.min(MAX_RETRIES);

        // 代理：resolve() 只落定文件值（含 Auto 跟随环境）；CLI/ENV 覆盖由宿主
        // 在 resolve() 之后用 resolve_proxy() 覆写 resolved.proxy（与 retry 同路）。
        let proxy = self.resolve_proxy(None, false);

        Ok(Resolved {
            model,
            base_url,
            api_key,
            provider_name,
            // 窗口按 provider 走(各服务商各配各的);顶层那个是历史遗留兜底
            context_window: pdef.context_window.or(self.context_window),
            protocol,
            session_header_enabled,
            retry,
            proxy,
        })
    }

    /// 代理最终落定。优先级：CLI(--proxy/--no-proxy) > ENV(ZNAIDE_NO_PROXY/ZNAIDE_PROXY)
    /// 文件 Off > 文件 Manual > 环境(HTTPS_PROXY…) > 直连。
    /// 非法值警告并按“该层没设”继续（与 ZNAIDE_PROTOCOL 同口径）。
    pub fn resolve_proxy(&self, cli_proxy: Option<String>, cli_no_proxy: bool) -> EffectiveProxy {
        if cli_no_proxy {
            return EffectiveProxy::Direct;
        }
        if let Some(u) = cli_proxy {
            let t = u.trim();
            if !t.is_empty() {
                match normalize_proxy_url(t) {
                    Ok(n) => return EffectiveProxy::Via(n),
                    Err(e) => eprintln!("⚠ --proxy={u:?} 无效({e:#})，已忽略"),
                }
            }
        }
        if let Some(v) = env_first(&["ZNAIDE_NO_PROXY"]) {
            if matches!(
                v.trim().to_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            ) {
                return EffectiveProxy::Direct;
            }
        }
        if self.proxy.mode == ProxyMode::Off {
            return EffectiveProxy::Direct;
        }
        if let Some(v) = env_first(&["ZNAIDE_PROXY"]) {
            let t = v.trim();
            if !t.is_empty() {
                match normalize_proxy_url(t) {
                    Ok(n) => return EffectiveProxy::Via(n),
                    Err(e) => eprintln!("⚠ 环境变量 ZNAIDE_PROXY={v:?} 无效({e:#})，已忽略"),
                }
            }
        }
        if self.proxy.mode == ProxyMode::Manual {
            match self.proxy.url.as_deref() {
                Some(u) if !u.trim().is_empty() => match normalize_proxy_url(u) {
                    Ok(n) => return EffectiveProxy::Via(n),
                    Err(e) => eprintln!("⚠ 配置文件 proxy.url={u:?} 无效({e:#})，已按跟随环境处理"),
                },
                _ => eprintln!("⚠ 配置文件代理为 manual 但未填地址，已按跟随环境处理"),
            }
        }
        // Auto：跟随环境（HTTPS_PROXY → ALL_PROXY → HTTP_PROXY，现状顺序）
        match crate::update::env_proxy_url() {
            Some(u) => EffectiveProxy::Via(u),
            None => EffectiveProxy::Direct,
        }
    }

    /// 弱网重试最终策略，优先级：CLI(--retry/--no-retry) > ENV > config 文件。
    /// ENV：ZNAIDE_NO_RETRY=1/true 强制关；ZNAIDE_RETRY=N(N>0 开 N 次，0 关)。
    /// 返回值已夹取 0..=MAX_RETRIES。
    pub fn resolve_retry(&self, cli_retry: Option<usize>, cli_no_retry: bool) -> RetryConfig {
        if cli_no_retry {
            return RetryConfig::disabled();
        }
        if let Some(n) = cli_retry {
            return RetryConfig::enabled_with(n);
        }
        if let Some(v) = env_first(&["ZNAIDE_NO_RETRY"]) {
            if matches!(v.trim().to_lowercase().as_str(), "1" | "true" | "yes" | "on") {
                return RetryConfig::disabled();
            }
        }
        if let Some(v) = env_first(&["ZNAIDE_RETRY"]) {
            let t = v.trim();
            if !t.is_empty() {
                match t.parse::<usize>() {
                    Ok(0) => return RetryConfig::disabled(),
                    Ok(n) => return RetryConfig::enabled_with(n),
                    Err(_) => eprintln!(
                        "⚠ 环境变量 ZNAIDE_RETRY={v:?} 无法识别(应为 0..=8 的整数),已忽略"
                    ),
                }
            }
        }
        let mut r = self.retry;
        r.max_retries = r.max_retries.min(MAX_RETRIES);
        r
    }
}

impl Resolved {
    /// 实际窗口:配置值 > 内置表;**都查不到 = None(未知)**,调用方据此只报绝对量、
    /// 不编百分比(见 tui 的 ctx 占用条)。
    pub fn effective_context_window(&self) -> Option<usize> {
        match self.context_window {
            Some(n) if n > 0 => Some(n),
            _ => model_context_window(&self.model),
        }
    }
}

impl Config {
    /// 单条消息的轮数上限:配置值 > 默认 200;0 表示不限(不替换成默认值)
    pub fn effective_max_turns(&self) -> usize {
        self.max_turns.unwrap_or(crate::session::DEFAULT_MAX_TURNS)
    }

    /// provider 清单(名字/base_url/模型),给 UI 列表用
    pub fn list_providers(&self) -> Vec<(String, String, String)> {
        let providers = self.all_providers();
        let mut out: Vec<(String, String, String)> = providers
            .into_iter()
            .map(|(name, def)| {
                let base = def.base_url.unwrap_or_else(|| "?".into());
                let model = def.model.unwrap_or_else(|| "(未设模型)".into());
                (name, base, model)
            })
            .collect();
        out.sort();
        out
    }

    /// 写盘前确保配置格式版本已打上(缺失则补当前版本)
    fn ensure_build_tag(&mut self) {
        if self.build_tag.is_none() {
            self.build_tag = Some(CONFIG_TAG.to_string());
        }
    }

    /// 把当前选择写进 config.json:值写进**对应 provider 条目**(表里没有就补一份),
    /// 不再写顶层三件套——顶层只作手动临时覆盖,面板保存不该把预设永久遮蔽掉。
    /// model/base_url 传 None 表示"保持该条目原值";api_key 传空串表示清除明文。
    /// context_window 传 None 表示"该条目不固定窗口"(按模型名查内置表,查不到 = 未知)。
    /// protocol 向导永远有确定值:Chat 存 None(= 默认值,config.json 保持干净,
    /// 老版本读新文件也不受影响)。
    /// session_header 直接存 bool(无"缺省即默认"的歧义，向导语义明确)。
    #[allow(clippy::too_many_arguments)]
    pub fn save(
        &mut self,
        provider: &str,
        model: Option<&str>,
        base_url: Option<&str>,
        api_key: Option<&str>,
        context_window: Option<usize>,
        protocol: ProtocolKind,
        session_header: bool,
    ) -> anyhow::Result<()> {
        self.ensure_build_tag();
        self.provider = Some(provider.to_string());
        {
            let entry = self.providers.entry(provider.to_string()).or_default();
            if let Some(m) = model {
                entry.model = Some(m.to_string());
            }
            if let Some(b) = base_url {
                entry.base_url = Some(b.to_string());
            }
            if let Some(k) = api_key {
                // 空串视为清除明文(api_key_env 保留,环境变量那条路仍可用)
                entry.api_key = Some(k.to_string()).filter(|s| !s.is_empty());
            }
            // 0/None 都算"不固定"(0 不是有效窗口,当没填)
            entry.context_window = context_window.filter(|n| *n > 0);
            entry.protocol = Some(protocol).filter(|p| *p != ProtocolKind::Chat);
            entry.session_header = session_header;
        }
        // 顶层三件套清空:留着会遮蔽预设,让"切 provider"失效
        self.model = None;
        self.base_url = None;
        self.api_key = None;
        self.protocol = None;
        self.persist()
    }

    /// 新增/覆盖一个 provider 预设并落盘
    pub fn save_provider(&mut self, name: &str, def: &ProviderDef) -> anyhow::Result<()> {
        self.ensure_build_tag();
        self.providers.insert(name.to_string(), def.clone());
        self.persist()
    }

    /// 设置全局人格并落盘(空串 = 关闭人格)
    pub fn save_persona(&mut self, name: &str) -> anyhow::Result<()> {
        self.ensure_build_tag();
        self.persona = if name.is_empty() {
            None
        } else {
            Some(name.to_string())
        };
        self.persist()
    }

    /// 按需直改(/cfg):只动目标字段,不碰全量 `save()` 的"重写整个条目+清空顶层"语义。
    /// 以下方法统一:ensure_build_tag → 改目标字段 → persist。`config.json` 零迁移。
    /// 弱网重试(顶层 retry)。超限夹取 MAX_RETRIES(与 resolve/apply_config 同口径)。
    pub fn save_retry(&mut self, retry: RetryConfig) -> anyhow::Result<()> {
        self.ensure_build_tag();
        let mut r = retry;
        r.max_retries = r.max_retries.min(MAX_RETRIES);
        self.retry = r;
        self.persist()
    }

    /// 网络代理（顶层 proxy）。mode 非 manual 时 url 强制 None（文件干净）。
    /// manual 非法地址拒绝落盘（调用方透出 anyhow 文案，面板回填重输）。
    pub fn save_proxy(&mut self, proxy: ProxyConfig) -> anyhow::Result<()> {
        self.ensure_build_tag();
        let mut p = proxy;
        p.url = p
            .url
            .map(|u| u.trim().to_string())
            .filter(|u| !u.is_empty());
        if p.mode != ProxyMode::Manual {
            p.url = None;
        }
        if p.mode == ProxyMode::Manual {
            // 非法地址拒绝落盘（面板回填重输；调用方透出 anyhow 文案）
            p.url = Some(normalize_proxy_url(p.url.as_deref().unwrap_or(""))?);
        }
        self.proxy = p;
        self.persist()
    }

    /// 轮次上限(顶层 max_turns)。None=默认,Some(0)=不限。
    pub fn save_max_turns(&mut self, max_turns: Option<usize>) -> anyhow::Result<()> {
        self.ensure_build_tag();
        self.max_turns = max_turns;
        self.persist()
    }

    /// 上下文窗口(当前家 provider 条目)。None/0=自动(不固定)。
    pub fn save_context_window(
        &mut self,
        provider: &str,
        w: Option<usize>,
    ) -> anyhow::Result<()> {
        self.ensure_build_tag();
        let entry = self.providers.entry(provider.to_string()).or_default();
        entry.context_window = w.filter(|n| *n > 0);
        self.persist()
    }

    /// API Key(当前家 provider 条目明文)。
    /// None=不动(env 家守卫:不把环境变量值落盘);Some("")=清空明文。
    pub fn save_api_key(
        &mut self,
        provider: &str,
        key: Option<&str>,
    ) -> anyhow::Result<()> {
        self.ensure_build_tag();
        if let Some(k) = key {
            let entry = self.providers.entry(provider.to_string()).or_default();
            entry.api_key = Some(k.to_string()).filter(|s| !s.is_empty());
        }
        self.persist()
    }

    /// 模型(当前家 provider 条目,不存在即建)。
    pub fn save_model(&mut self, provider: &str, model: &str) -> anyhow::Result<()> {
        self.ensure_build_tag();
        let entry = self.providers.entry(provider.to_string()).or_default();
        entry.model = Some(model.to_string());
        self.persist()
    }

    /// 端点(当前家 provider 条目,不存在即建)。必须 http(s):// 开头。
    pub fn save_base_url(&mut self, provider: &str, url: &str) -> anyhow::Result<()> {
        let t = url.trim();
        if !(t.starts_with("http://") || t.starts_with("https://")) {
            anyhow::bail!("端点要以 http:// 或 https:// 开头");
        }
        self.ensure_build_tag();
        let entry = self.providers.entry(provider.to_string()).or_default();
        entry.base_url = Some(t.to_string());
        self.persist()
    }

    /// 协议(当前家 provider 条目)。Chat 存 None(config.json 保持干净,老版本可读)。
    pub fn save_protocol(&mut self, provider: &str, p: ProtocolKind) -> anyhow::Result<()> {
        self.ensure_build_tag();
        let entry = self.providers.entry(provider.to_string()).or_default();
        entry.protocol = Some(p).filter(|p| *p != ProtocolKind::Chat);
        self.persist()
    }

    /// 会话头开关(当前家 provider 条目)。
    pub fn save_session_header(&mut self, provider: &str, on: bool) -> anyhow::Result<()> {
        self.ensure_build_tag();
        let entry = self.providers.entry(provider.to_string()).or_default();
        entry.session_header = on;
        self.persist()
    }

    /// 切换当前服务商(只换顶层指针;条目不存在则建空条目,不碰别家字段)。
    pub fn switch_provider(&mut self, name: &str) -> anyhow::Result<()> {
        self.ensure_build_tag();
        self.provider = Some(name.to_string());
        self.providers.entry(name.to_string()).or_default();
        self.persist()
    }

    /// 落盘(整份序列化写入)
    fn persist(&self) -> anyhow::Result<()> {
        let path = config_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, text + "\n")?;
        Ok(())
    }

    /// 配置格式标记需要迁移:build_tag 存在且不是当前版本(他版写入/外部改动)
    pub fn needs_migrate(&self) -> bool {
        matches!(&self.build_tag, Some(t) if t != CONFIG_TAG)
    }

    /// 执行迁移:把 build_tag 置回当前版本,并把历史遗留的"顶层三件套 + 顶层窗口"
    /// 搬进当前 provider 条目(它们只是手动临时覆盖,长期留着会遮蔽预设、让切 provider 失效)。
    /// 返回是否发生过迁移。
    pub fn migrate(&mut self) -> anyhow::Result<bool> {
        if !self.needs_migrate() {
            return Ok(false);
        }
        self.build_tag = Some(CONFIG_TAG.to_string());
        self.absorb_top_level_into_provider();
        self.persist()?;
        Ok(true)
    }

    /// 把顶层 model/base_url/api_key/context_window 搬进当前 provider 条目并清空顶层。
    /// 口径统一后顶层三件套一律"手动覆盖"(优先于预设),所以"搬进条目 + 清空顶层"
    /// 之后 `resolve()` 的结果必然与搬之前一致(见 migrate_absorbs_top_level_overrides)。
    fn absorb_top_level_into_provider(&mut self) {
        if self.model.is_none()
            && self.base_url.is_none()
            && self.api_key.is_none()
            && self.context_window.is_none()
            && self.protocol.is_none()
        {
            return;
        }
        let name = self
            .provider
            .clone()
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| "ollama".to_string());
        let entry = self.providers.entry(name).or_default();
        if let Some(m) = self.model.take() {
            entry.model = Some(m);
        }
        if let Some(b) = self.base_url.take() {
            entry.base_url = Some(b);
        }
        if let Some(cw) = self.context_window.take() {
            entry.context_window = Some(cw);
        }
        // 口径统一后顶层三件套一律"手动覆盖"(优先于预设),所以顶层 key 一定有生效,
        // 直接搬进条目即可——搬完生效值不变。
        if let Some(k) = self.api_key.take() {
            entry.api_key = Some(k);
        }
        if let Some(p) = self.protocol.take() {
            entry.protocol = Some(p);
        }
    }
}

/// 是否已存在配置文件
pub fn config_exists() -> bool {
    config_path().exists()
}

/// resolve() 会读取的环境变量里,有没有哪个已经设了非空值。
/// 宿主判断"用户是不是已经提供了配置"时用它,别再各自手写清单——手写那份漏了
/// `ZNAIDE_PROVIDER`(单给一个 provider 名就能靠内置预设跑起来,却会被判成"没配置")。
pub fn env_configured() -> bool {
    let names = [
        "ZNAIDE_PROVIDER",
        "ZNAIDE_MODEL",
        "OPENAI_MODEL",
        "ZNAIDE_BASE_URL",
        "OPENAI_BASE_URL",
        "ZNAIDE_API_KEY",
        "OPENAI_API_KEY",
        "ZNAIDE_PROXY",
    ];
    names.iter().any(|n| {
        std::env::var(n)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
    })
}

/// 建数据目录(memories/skills/sessions),不生成任何配置文件;
/// 首次创建返回 true(引导首启用用)
pub fn ensure_data_dirs() -> anyhow::Result<bool> {
    let first = !data_dir().exists();
    for sub in ["memories", "skills", "sessions"] {
        std::fs::create_dir_all(data_dir().join(sub))?;
    }
    Ok(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_resolve_uses_ollama_preset() {
        // 空配置:默认 provider=ollama 自带 base_url+model
        let cfg = Config::default();
        let r = cfg.resolve(None, None, None, None, None, None).unwrap();
        assert_eq!(r.provider_name, "ollama");
        assert_eq!(r.base_url, "http://localhost:11434/v1");
        assert_eq!(r.model, "qwen3:8b");
    }

    #[test]
    fn cli_overrides_config() {
        let cfg = Config {
            model: Some("cfg-model".into()),
            base_url: Some("http://cfg".into()),
            api_key: None,
            provider: None,
            context_window: None,
            max_turns: None,
            persona: None,
            protocol: None,
            retry: RetryConfig::disabled(),
            proxy: ProxyConfig::default(),
            build_tag: None,
            providers: Default::default(),
        };
        let r = cfg
            .resolve(Some("cli-model".into()), None, None, None, None, None)
            .unwrap();
        assert_eq!(r.model, "cli-model");
        assert_eq!(r.base_url, "http://cfg");
        assert_eq!(r.provider_name, "ollama");
    }

    #[test]
    fn provider_preset_applies() {
        let cfg = Config::default();
        // 指定 dashscope,无顶层 model → 用预设模型与 key env
        let r = cfg
            .resolve(None, None, None, Some("dashscope".into()), None, None)
            .unwrap();
        assert_eq!(
            r.base_url,
            "https://dashscope.aliyuncs.com/compatible-mode/v1"
        );
        assert_eq!(r.model, "qwen-plus");
        // api_key 从 env 读不到时应为 None(不报错)
    }

    #[test]
    fn user_provider_overrides_builtin() {
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "ollama".into(),
            ProviderDef {
                base_url: Some("http://127.0.0.1:8080/v1".into()),
                model: Some("llama3".into()),
                ..Default::default()
            },
        );
        let cfg = Config {
            model: None,
            base_url: None,
            api_key: None,
            provider: Some("ollama".into()),
            context_window: None,
            max_turns: None,
            persona: None,
            protocol: None,
            retry: RetryConfig::disabled(),
            proxy: ProxyConfig::default(),
            build_tag: None,
            providers,
        };
        let r = cfg.resolve(None, None, None, None, None, None).unwrap();
        assert_eq!(r.base_url, "http://127.0.0.1:8080/v1");
        assert_eq!(r.model, "llama3");
    }

    #[test]
    fn context_window_defaults_from_model_table() {
        // 配置优先
        let mut cfg = Config {
            model: Some("qwen3:8b".into()),
            context_window: Some(65536),
            ..Default::default()
        };
        let r = cfg.resolve(None, None, None, None, None, None).unwrap();
        assert_eq!(r.effective_context_window(), Some(65536));
        // 未配置 → 按模型名匹配(2026-09 检索值)
        cfg.context_window = None;
        let r = cfg.resolve(None, None, None, None, None, None).unwrap();
        assert_eq!(r.effective_context_window(), Some(40960)); // ollama qwen3:8b
                                                               // 云端/闭源家族
        assert_eq!(model_context_window("qwen-plus"), Some(1_000_000));
        assert_eq!(model_context_window("claude-sonnet-4-5"), Some(200_000));
        assert_eq!(model_context_window("gpt-5"), Some(400_000));
        assert_eq!(model_context_window("deepseek-v4-pro"), Some(1_048_576));
        // v4 代短名(真实 API id 不带 v4 时的别名)
        assert_eq!(model_context_window("deepseek-flash"), Some(1_048_576));
        // 云端 Qwen3 代 max:不能被 ollama 的 qwen3 兜底(40k)吞掉
        assert_eq!(model_context_window("qwen3.8-max"), Some(1_000_000));
        assert_eq!(model_context_window("qwen3-max"), Some(1_000_000));
        assert_eq!(model_context_window("qwen3:8b"), Some(40_960));
    }

    /// B:认不出来的模型返回 None(未知),不再假装 32k——否则百万级模型会被算成快满了
    #[test]
    fn unknown_model_window_is_none_not_guess() {
        assert_eq!(model_context_window("my-custom-model"), None);
        assert_eq!(
            model_context_window("deepseek-flash-v9-unknown"),
            Some(1_048_576)
        ); // 命中别名
        let cfg = Config {
            model: Some("my-custom-model".into()),
            ..Default::default()
        };
        let r = cfg.resolve(None, None, None, None, None, None).unwrap();
        assert_eq!(
            r.effective_context_window(),
            None,
            "查不到 = 未知,交给调用方只说绝对量"
        );
        // 面板/配置里写死就照写死(0 无效,当没填)
        let cfg2 = Config {
            model: Some("my-custom-model".into()),
            context_window: Some(262_144),
            ..Default::default()
        };
        let r2 = cfg2.resolve(None, None, None, None, None, None).unwrap();
        assert_eq!(r2.effective_context_window(), Some(262_144));
    }

    #[test]
    fn effective_max_turns_defaults_and_overrides() {
        let mut cfg = Config::default();
        assert_eq!(cfg.effective_max_turns(), crate::session::DEFAULT_MAX_TURNS);
        cfg.max_turns = Some(500);
        assert_eq!(cfg.effective_max_turns(), 500);
        // 0 是"不限"的有效值,不能被替换成默认
        cfg.max_turns = Some(0);
        assert_eq!(cfg.effective_max_turns(), 0);
    }

    /// 轮数上限随 save() 落盘并读回——配置面板保存走的就是 `cfg.max_turns = …; save(…)`
    /// 这条路;顺带覆盖"老配置没有该键 → None"。
    #[test]
    fn max_turns_roundtrip_through_save() {
        let _g = crate::test_util::DATA_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("znaide_cfg_mt_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ZNAIDE_DATA_DIR", &dir);

        // 全新(无 config.json):未设置
        let mut cfg = Config::load().unwrap_or_default();
        assert_eq!(cfg.max_turns, None);

        // 面板保存:先设值再 save → 文件里能读回
        cfg.max_turns = Some(500);
        cfg.save(
            "ollama",
            Some("qwen3:8b"),
            Some("http://127.0.0.1:11434/v1"),
            None,
            None,
            ProtocolKind::Chat,
            false,
        )
        .unwrap();
        assert_eq!(Config::load().unwrap().max_turns, Some(500));

        // 清空 → 写回 None(回到默认)
        let mut cfg = Config::load().unwrap();
        cfg.max_turns = None;
        cfg.save(
            "ollama",
            Some("qwen3:8b"),
            Some("http://127.0.0.1:11434/v1"),
            None,
            None,
            ProtocolKind::Chat,
            false,
        )
        .unwrap();
        assert_eq!(Config::load().unwrap().max_turns, None);

        // 老配置没有该键照常读
        let legacy: Config = serde_json::from_str(r#"{"model":"m"}"#).unwrap();
        assert_eq!(legacy.max_turns, None);

        std::env::remove_var("ZNAIDE_DATA_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// build_tag(配置格式版本):老配置无此键读为 None;写盘前由 save 系补上
    #[test]
    fn build_tag_roundtrip_and_legacy_default() {
        let old: Config = serde_json::from_str(r#"{"model":"m"}"#).unwrap();
        assert_eq!(old.build_tag, None, "老配置(无 build_tag)应读为 None");
        // 用常量拼,别再写死字面量(换版本时会一起失效)
        let with_tag: Config =
            serde_json::from_str(&format!(r#"{{"build_tag":"{CONFIG_TAG}"}}"#)).unwrap();
        assert_eq!(with_tag.build_tag.as_deref(), Some(CONFIG_TAG));
    }

    /// 迁移判定:当前版本/无标记不触发;旧版与异源标记都触发(启动时自动置回)
    #[test]
    fn migrate_detects_foreign_build_tag() {
        let cur: Config =
            serde_json::from_str(&format!(r#"{{"build_tag":"{CONFIG_TAG}"}}"#)).unwrap();
        assert!(!cur.needs_migrate(), "当前版本标记不需迁移");
        assert!(!Config::default().needs_migrate(), "无标记(初次)不需迁移");
        let v1: Config = serde_json::from_str(r#"{"build_tag":"a16416a02578"}"#).unwrap();
        assert!(v1.needs_migrate(), "上一版标记应触发一次性迁移");
        let foreign: Config = serde_json::from_str(r#"{"build_tag":"deadbeef00"}"#).unwrap();
        assert!(foreign.needs_migrate(), "异源标记应触发迁移");
    }

    /// D4:用户条目按字段覆盖内置预设,不能顺手丢掉它没写的字段
    #[test]
    fn user_preset_merges_field_wise() {
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "deepseek".into(),
            ProviderDef {
                base_url: Some("https://mirror.example.com/v1".into()),
                ..Default::default()
            },
        );
        let cfg = Config {
            providers,
            ..Default::default()
        };
        let all = cfg.all_providers();
        let d = all.get("deepseek").expect("内置 deepseek 应在");
        assert_eq!(
            d.base_url.as_deref(),
            Some("https://mirror.example.com/v1"),
            "用户写的覆盖"
        );
        assert!(d.model.is_some(), "没写的 model 应保留内置值");
        assert!(d.api_key_env.is_some(), "没写的 api_key_env 应保留内置值");
        // 用户自定义的新 provider 照常加入
        let mut providers2 = std::collections::HashMap::new();
        providers2.insert(
            "myprov".into(),
            ProviderDef {
                base_url: Some("https://mine/v1".into()),
                model: Some("m".into()),
                ..Default::default()
            },
        );
        let cfg2 = Config {
            providers: providers2,
            ..Default::default()
        };
        assert!(cfg2.all_providers().contains_key("myprov"));
    }

    /// D3:上下文窗口按 provider 走,顶层那个只作兜底
    #[test]
    fn context_window_prefers_provider_entry() {
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "ollama".into(),
            ProviderDef {
                base_url: Some("http://127.0.0.1:11434/v1".into()),
                model: Some("qwen3:8b".into()),
                context_window: Some(40_960),
                ..Default::default()
            },
        );
        let cfg = Config {
            providers,
            context_window: Some(9_999_999), // 历史遗留的全局值:不该盖过 provider 自己的
            ..Default::default()
        };
        let r = cfg
            .resolve(None, None, None, Some("ollama".into()), None, None)
            .unwrap();
        assert_eq!(r.effective_context_window(), Some(40_960));
        // 没配 provider 窗口时,退回顶层,再退回模型表
        let cfg2 = Config {
            context_window: Some(65_536),
            ..Default::default()
        };
        let r2 = cfg2
            .resolve(None, None, None, Some("ollama".into()), None, None)
            .unwrap();
        assert_eq!(r2.effective_context_window(), Some(65_536));
    }

    /// D1:面板保存写进 provider 条目,不再把顶层三件套写死(否则切 provider 失效)
    #[test]
    fn save_writes_into_provider_entry_not_top_level() {
        let _g = crate::test_util::DATA_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("znaide_cfg_save_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ZNAIDE_DATA_DIR", &dir);

        let mut cfg = Config::default();
        cfg.save(
            "deepseek",
            Some("deepseek-chat"),
            Some("https://api.deepseek.com/v1"),
            Some("sk-1"),
            Some(131_072),
            ProtocolKind::Chat,
            false,
        )
        .unwrap();

        assert_eq!(cfg.model, None, "顶层 model 不该被写");
        assert_eq!(cfg.base_url, None, "顶层 base_url 不该被写");
        assert_eq!(cfg.api_key, None, "顶层 api_key 不该被写");
        let d = cfg.providers.get("deepseek").expect("条目应写入");
        assert_eq!(d.model.as_deref(), Some("deepseek-chat"));
        assert_eq!(d.base_url.as_deref(), Some("https://api.deepseek.com/v1"));
        assert_eq!(d.api_key.as_deref(), Some("sk-1"));
        assert_eq!(
            d.context_window,
            Some(131_072),
            "窗口也写进条目(多服务商各配各的)"
        );

        // 落盘后再读,值仍在条目里 → 切 provider 时不会被顶层遮蔽
        let back = Config::load().unwrap();
        assert!(back.model.is_none() && back.base_url.is_none());
        assert_eq!(
            back.all_providers()
                .get("deepseek")
                .and_then(|d| d.model.clone()),
            Some("deepseek-chat".to_string())
        );
        assert_eq!(
            back.all_providers()
                .get("deepseek")
                .and_then(|d| d.context_window),
            Some(131_072)
        );

        // 面板留空 = 该条目不固定窗口(清掉旧值,回到按模型名查表)
        let mut cfg = back;
        cfg.save(
            "deepseek",
            Some("deepseek-chat"),
            Some("https://api.deepseek.com/v1"),
            None,
            None,
            ProtocolKind::Chat,
            false,
        )
        .unwrap();
        assert_eq!(
            cfg.providers.get("deepseek").and_then(|d| d.context_window),
            None
        );
        assert_eq!(
            Config::load()
                .unwrap()
                .providers
                .get("deepseek")
                .and_then(|d| d.context_window),
            None
        );

        std::env::remove_var("ZNAIDE_DATA_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// D5:自愈迁移把顶层三件套搬进当前 provider,且**迁移前后生效配置一致**
    #[test]
    fn migrate_absorbs_top_level_overrides() {
        const VAR: &str = "ZNAIDE_TEST_ABSORB_KEY";
        std::env::remove_var(VAR); // 分支 A:环境变量不存在
        let raw = format!(
            r#"{{
            "model": "deepseek-flash",
            "base_url": "https://api.deepseek.com/v1",
            "api_key": "sk-top",
            "provider": "deepseek",
            "context_window": 65536,
            "providers": {{
                "deepseek": {{"model": "deepseek-v4-flash", "api_key_env": "{VAR}"}},
                "ollama": {{"base_url": "http://127.0.0.1:11434/v1", "model": "qwen3:8b"}}
            }}
        }}"#
        );
        let before: Config = serde_json::from_str(&raw).unwrap();
        let eff_before = before.resolve(None, None, None, None, None, None).unwrap();

        let mut after: Config = serde_json::from_str(&raw).unwrap();
        after.build_tag = Some(CONFIG_TAG.to_string());
        after.absorb_top_level_into_provider();

        assert!(after.model.is_none() && after.base_url.is_none() && after.api_key.is_none());
        assert!(after.context_window.is_none(), "顶层窗口也该搬走");
        let d = after.providers.get("deepseek").unwrap();
        assert_eq!(
            d.model.as_deref(),
            Some("deepseek-flash"),
            "顶层值生效过 → 覆盖条目"
        );
        assert_eq!(d.base_url.as_deref(), Some("https://api.deepseek.com/v1"));
        assert_eq!(d.context_window, Some(65_536));
        assert_eq!(
            d.api_key.as_deref(),
            Some("sk-top"),
            "条目取不到 key 时,顶层明文要搬进来(否则用户丢 key)"
        );

        // 迁移前后生效参数必须一模一样
        let eff_after = after.resolve(None, None, None, None, None, None).unwrap();
        assert_eq!(eff_before.model, eff_after.model);
        assert_eq!(eff_before.base_url, eff_after.base_url);
        assert_eq!(eff_before.api_key, eff_after.api_key);
        assert_eq!(
            eff_before.effective_context_window(),
            eff_after.effective_context_window()
        );
        // 另一个 provider 不受影响,切过去仍好使(这正是本次修复的目的)
        let other = after
            .resolve(None, None, None, Some("ollama".into()), None, None)
            .unwrap();
        assert_eq!(other.model, "qwen3:8b");
        assert_eq!(other.base_url, "http://127.0.0.1:11434/v1");

        // 分支 B:顶层与条目都有 key 时,顶层优先(口径已统一为"顶层 = 手动覆盖"),
        // 迁移把顶层值搬进条目 → 生效 key 前后一致
        let raw_b = r#"{
            "api_key": "sk-top",
            "provider": "ollama",
            "providers": { "ollama": {"model": "qwen3:8b", "api_key": "sk-old"} }
        }"#;
        let mut b: Config = serde_json::from_str(raw_b).unwrap();
        let eff_b = b.resolve(None, None, None, None, None, None).unwrap();
        assert_eq!(
            eff_b.api_key.as_deref(),
            Some("sk-top"),
            "顶层覆盖预设(三个字段口径一致)"
        );
        b.absorb_top_level_into_provider();
        assert_eq!(
            b.providers.get("ollama").unwrap().api_key.as_deref(),
            Some("sk-top")
        );
        assert_eq!(
            b.resolve(None, None, None, None, None, None)
                .unwrap()
                .api_key
                .as_deref(),
            Some("sk-top"),
            "搬完生效 key 不变"
        );
    }

    /// 协议默认值:空配置 → Chat;`ProtocolKind::default()` 也是 Chat
    #[test]
    fn protocol_default_chat() {
        assert_eq!(ProtocolKind::default(), ProtocolKind::Chat);
        let cfg = Config::default();
        let r = cfg.resolve(None, None, None, None, None, None).unwrap();
        assert_eq!(r.protocol, ProtocolKind::Chat);
    }

    /// 协议按 provider 走:A=Response、B 缺省 → 各自生效;字段级合并(只写 base_url
    /// 的用户条目不丢内置 protocol…反之亦然:只写 protocol 的条目不丢内置 base_url)
    #[test]
    fn protocol_per_provider() {
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "a".into(),
            ProviderDef {
                model: Some("model-a".into()),
                protocol: Some(ProtocolKind::Response),
                ..Default::default()
            },
        );
        let cfg = Config {
            providers,
            ..Default::default()
        };
        let ra = cfg
            .resolve(None, None, None, Some("a".into()), None, None)
            .unwrap();
        assert_eq!(ra.protocol, ProtocolKind::Response);
        // 只写 protocol 的条目:base_url 回退默认,不 panic
        assert!(!ra.base_url.is_empty());
        let rb = cfg
            .resolve(None, None, None, Some("ollama".into()), None, None)
            .unwrap();
        assert_eq!(rb.protocol, ProtocolKind::Chat);

        // 只写 base_url 的用户条目不丢内置其他字段(字段级合并不断言 protocol,
        // 内置全是 None→Chat;这里只确认合并逻辑没把条目整条替换)
        let mut providers2 = std::collections::HashMap::new();
        providers2.insert(
            "deepseek".into(),
            ProviderDef {
                base_url: Some("https://mirror.example.com/v1".into()),
                protocol: Some(ProtocolKind::Response),
                ..Default::default()
            },
        );
        let cfg2 = Config {
            providers: providers2,
            ..Default::default()
        };
        let all = cfg2.all_providers();
        let d = all.get("deepseek").unwrap();
        assert_eq!(d.base_url.as_deref(), Some("https://mirror.example.com/v1"));
        assert_eq!(d.protocol, Some(ProtocolKind::Response));
        assert!(d.model.is_some(), "没写的 model 应保留内置值");
    }

    /// env 与 CLI 优先级:env 生效;CLI 覆盖 env;顶层覆盖条目
    #[test]
    fn protocol_env_and_cli() {
        let _g = crate::test_util::DATA_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("ZNAIDE_PROTOCOL", "response");
        let cfg = Config::default();
        let r = cfg.resolve(None, None, None, None, None, None).unwrap();
        assert_eq!(r.protocol, ProtocolKind::Response);
        // CLI 覆盖 env
        let r2 = cfg
            .resolve(None, None, None, None, Some(ProtocolKind::Chat), None)
            .unwrap();
        assert_eq!(r2.protocol, ProtocolKind::Chat);
        std::env::remove_var("ZNAIDE_PROTOCOL");

        // 顶层覆盖条目
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "ollama".into(),
            ProviderDef {
                protocol: Some(ProtocolKind::Response),
                ..Default::default()
            },
        );
        let cfg3 = Config {
            protocol: Some(ProtocolKind::Chat),
            providers,
            ..Default::default()
        };
        let r3 = cfg3.resolve(None, None, None, None, None, None).unwrap();
        assert_eq!(r3.protocol, ProtocolKind::Chat, "顶层手动覆盖优先于条目");
    }

    /// save 落盘:Response 存入 → 读回 Some(Response);Chat 存入 → 条目 None(干净性);
    /// 顶层 protocol 被清空
    #[test]
    fn protocol_save_roundtrip() {
        let _g = crate::test_util::DATA_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("znaide_cfg_proto_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ZNAIDE_DATA_DIR", &dir);

        let mut cfg = Config::load().unwrap_or_default();
        cfg.protocol = Some(ProtocolKind::Response); // 顶层残留应被清空
        cfg.save(
            "ollama",
            Some("qwen3:8b"),
            Some("http://127.0.0.1:11434/v1"),
            None,
            None,
            ProtocolKind::Response,
            false,
        )
        .unwrap();
        assert_eq!(cfg.protocol, None, "顶层 protocol 随保存清空");
        let back = Config::load().unwrap();
        assert_eq!(
            back.providers.get("ollama").and_then(|d| d.protocol),
            Some(ProtocolKind::Response)
        );
        let r = back.resolve(None, None, None, None, None, None).unwrap();
        assert_eq!(r.protocol, ProtocolKind::Response);

        // Chat 存入 → 条目 None(干净性:老版本读新文件不受影响)
        let mut cfg2 = back;
        cfg2.save(
            "ollama",
            Some("qwen3:8b"),
            Some("http://127.0.0.1:11434/v1"),
            None,
            None,
            ProtocolKind::Chat,
            false,
        )
        .unwrap();
        assert_eq!(
            Config::load()
                .unwrap()
                .providers
                .get("ollama")
                .and_then(|d| d.protocol),
            None
        );

        std::env::remove_var("ZNAIDE_DATA_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 老配置(无 protocol 键)零迁移:读为 None → resolve 得 Chat
    #[test]
    fn legacy_config_no_protocol() {
        let legacy: Config = serde_json::from_str(r#"{"model":"m"}"#).unwrap();
        assert_eq!(legacy.protocol, None);
        let r = legacy.resolve(None, None, None, None, None, None).unwrap();
        assert_eq!(r.protocol, ProtocolKind::Chat);
        // 条目里的老格式同样
        let legacy2: Config = serde_json::from_str(r#"{"providers":{"x":{"model":"m"}}}"#).unwrap();
        assert_eq!(legacy2.providers["x"].protocol, None);
    }

    /// 非法值 → None;复数/大小写也认
    #[test]
    fn parse_invalid_protocol() {
        assert_eq!(ProtocolKind::parse("foo"), None);
        assert_eq!(ProtocolKind::parse(""), None);
        assert_eq!(
            ProtocolKind::parse("Responses"),
            Some(ProtocolKind::Response)
        );
        assert_eq!(
            ProtocolKind::parse("RESPONSE"),
            Some(ProtocolKind::Response)
        );
        assert_eq!(ProtocolKind::parse("chat"), Some(ProtocolKind::Chat));
        assert_eq!(ProtocolKind::parse("CHAT"), Some(ProtocolKind::Chat));
        assert_eq!(ProtocolKind::Chat.as_str(), "chat");
        assert_eq!(ProtocolKind::Response.as_str(), "response");
    }

    /// 会话头开关:老配置缺键默认关(零回归)
    #[test]
    fn session_header_legacy_defaults_off() {
        let legacy: Config = serde_json::from_str(r#"{"model":"m"}"#).unwrap();
        assert!(!legacy
            .providers
            .get("x")
            .map(|d| d.session_header)
            .unwrap_or(false));
        assert!(!ProviderDef::default().session_header);
        let r = legacy.resolve(None, None, None, None, None, None).unwrap();
        assert!(!r.session_header_enabled, "老配置无该键 → 不发头");
    }

    /// 会话头开关:字段级合并只"或"(用户开 → 合并后开)
    #[test]
    fn session_header_merges_or() {
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "ollama".into(),
            ProviderDef {
                session_header: true,
                ..Default::default()
            },
        );
        let cfg = Config {
            providers,
            ..Default::default()
        };
        assert!(cfg.all_providers()["ollama"].session_header);
        let r = cfg.resolve(None, None, None, None, None, None).unwrap();
        assert!(r.session_header_enabled);
        // 用户没写 → 内置 false 保留
        let cfg2 = Config::default();
        assert!(!cfg2.all_providers()["ollama"].session_header);
    }

    /// 会话头开关:env 解析(1/true/yes/on 开;0/false/no/off 关;非法忽略)
    #[test]
    fn session_header_env_parsing() {
        assert_eq!(parse_session_header_env("1"), Some(true));
        assert_eq!(parse_session_header_env("true"), Some(true));
        assert_eq!(parse_session_header_env("YES"), Some(true));
        assert_eq!(parse_session_header_env("on"), Some(true));
        assert_eq!(parse_session_header_env("0"), Some(false));
        assert_eq!(parse_session_header_env("false"), Some(false));
        assert_eq!(parse_session_header_env("No"), Some(false));
        assert_eq!(parse_session_header_env("off"), Some(false));
        assert_eq!(parse_session_header_env(""), None);
        assert_eq!(parse_session_header_env("maybe"), None);
    }

    /// 会话头开关:env 生效;CLI 压住 env/文件
    #[test]
    fn session_header_env_and_cli_precedence() {
        let _g = crate::test_util::DATA_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // 文件开
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "ollama".into(),
            ProviderDef {
                session_header: true,
                ..Default::default()
            },
        );
        let cfg = Config {
            providers,
            ..Default::default()
        };
        // 无 env/CLI → 文件值
        std::env::remove_var("ZNAIDE_SESSION_HEADER");
        let r = cfg.resolve(None, None, None, None, None, None).unwrap();
        assert!(r.session_header_enabled);
        // env 关 → 压住文件开
        std::env::set_var("ZNAIDE_SESSION_HEADER", "0");
        let r = cfg.resolve(None, None, None, None, None, None).unwrap();
        assert!(!r.session_header_enabled);
        // CLI 开 → 压住 env 关
        let r = cfg
            .resolve(None, None, None, None, None, Some(true))
            .unwrap();
        assert!(r.session_header_enabled);
        // CLI 关 → 压住文件开
        std::env::remove_var("ZNAIDE_SESSION_HEADER");
        let r = cfg
            .resolve(None, None, None, None, None, Some(false))
            .unwrap();
        assert!(!r.session_header_enabled);
        std::env::remove_var("ZNAIDE_SESSION_HEADER");
    }

    /// 会话头开关:save 落盘读回
    #[test]
    fn session_header_save_roundtrip() {
        let _g = crate::test_util::DATA_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("znaide_cfg_sh_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ZNAIDE_DATA_DIR", &dir);

        let mut cfg = Config::load().unwrap_or_default();
        cfg.save(
            "deepseek",
            Some("deepseek-chat"),
            Some("https://api.deepseek.com/v1"),
            None,
            None,
            ProtocolKind::Chat,
            true,
        )
        .unwrap();
        let back = Config::load().unwrap();
        assert_eq!(
            back.providers.get("deepseek").map(|d| d.session_header),
            Some(true)
        );
        std::env::remove_var("ZNAIDE_SESSION_HEADER");
        let r = back.resolve(None, None, None, None, None, None).unwrap();
        assert!(r.session_header_enabled);

        // 关 → 写回 false
        let mut cfg = back;
        cfg.save(
            "deepseek",
            Some("deepseek-chat"),
            Some("https://api.deepseek.com/v1"),
            None,
            None,
            ProtocolKind::Chat,
            false,
        )
        .unwrap();
        assert_eq!(
            Config::load()
                .unwrap()
                .providers
                .get("deepseek")
                .map(|d| d.session_header),
            Some(false)
        );

        std::env::remove_var("ZNAIDE_DATA_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 弱网重试默认关闭；resolve() 落定文件值
    #[test]
    fn retry_defaults_to_disabled() {
        let cfg = Config::default();
        assert_eq!(cfg.retry.effective_times(), 0);
        let r = cfg.resolve(None, None, None, None, None, None).unwrap();
        assert_eq!(r.retry.effective_times(), 0);
    }

    /// resolve_retry 优先级：CLI > ENV > config 文件；超限夹取 8
    #[test]
    fn retry_cli_env_config_priority() {
        let _g = crate::test_util::DATA_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("ZNAIDE_RETRY");
        std::env::remove_var("ZNAIDE_NO_RETRY");
        let mut cfg = Config::default();
        cfg.retry = RetryConfig::enabled_with(5);
        // config 文件值
        assert_eq!(cfg.resolve_retry(None, false).effective_times(), 5);
        // ENV 覆盖 config
        std::env::set_var("ZNAIDE_RETRY", "3");
        assert_eq!(cfg.resolve_retry(None, false).effective_times(), 3);
        // CLI 覆盖 ENV
        assert_eq!(cfg.resolve_retry(Some(8), false).effective_times(), 8);
        // 超限夹取
        assert_eq!(cfg.resolve_retry(Some(99), false).effective_times(), 8);
        // --no-retry / ENV 关闭优先
        assert_eq!(cfg.resolve_retry(Some(5), true).effective_times(), 0);
        std::env::remove_var("ZNAIDE_RETRY");
        std::env::set_var("ZNAIDE_NO_RETRY", "1");
        assert_eq!(cfg.resolve_retry(None, false).effective_times(), 0);
        std::env::remove_var("ZNAIDE_NO_RETRY");
    }

    /// need03 U1:save_retry 夹取(99→8);disabled 生效 0 次
    #[test]
    fn quick_save_retry_clamps() {
        assert_eq!(RetryConfig::enabled_with(99).max_retries, MAX_RETRIES);
        assert_eq!(RetryConfig::disabled().effective_times(), 0);
        let _g = crate::test_util::DATA_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("znaide_qc_u1_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ZNAIDE_DATA_DIR", &dir);
        let mut cfg = Config::default();
        cfg.save_retry(RetryConfig::enabled_with(99)).unwrap();
        let back = Config::load().unwrap();
        assert!(back.retry.enabled);
        assert_eq!(back.retry.max_retries, MAX_RETRIES);
        assert_eq!(back.retry.effective_times(), MAX_RETRIES);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// need03 U2:save_max_turns 语义(None→默认200;Some(0)=不限;Some(5)=5)
    #[test]
    fn quick_save_max_turns_semantics() {
        let _g = crate::test_util::DATA_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("znaide_qc_u2_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ZNAIDE_DATA_DIR", &dir);
        let mut cfg = Config::default();
        cfg.save_max_turns(None).unwrap();
        assert_eq!(
            Config::load().unwrap().effective_max_turns(),
            crate::session::DEFAULT_MAX_TURNS
        );
        cfg.save_max_turns(Some(0)).unwrap();
        assert_eq!(Config::load().unwrap().effective_max_turns(), 0);
        cfg.save_max_turns(Some(5)).unwrap();
        assert_eq!(Config::load().unwrap().effective_max_turns(), 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// need03 U3:save_context_window(Some(0)→None;None→模型表;正整数直存)
    #[test]
    fn quick_save_context_window_semantics() {
        let _g = crate::test_util::DATA_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("znaide_qc_u3_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ZNAIDE_DATA_DIR", &dir);
        let mut cfg = Config::default();
        // 0 视为自动
        cfg.save_context_window("ollama", Some(0)).unwrap();
        assert_eq!(Config::load().unwrap().providers["ollama"].context_window, None);
        // 正整数直存
        cfg.save_context_window("ollama", Some(65536)).unwrap();
        assert_eq!(
            Config::load().unwrap().providers["ollama"].context_window,
            Some(65536)
        );
        // None → 按模型表(qwen3:8b=40960)
        cfg.save_context_window("ollama", None).unwrap();
        let back = Config::load().unwrap();
        assert_eq!(back.providers["ollama"].context_window, None);
        let r = back.resolve(None, None, None, None, None, None).unwrap();
        assert_eq!(r.effective_context_window(), Some(40960));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// need03 U4:save_protocol(Chat→条目None;Response→Some;parse 复数兼容)
    #[test]
    fn quick_save_protocol_semantics() {
        assert_eq!(ProtocolKind::parse("RESPONSES"), Some(ProtocolKind::Response));
        let _g = crate::test_util::DATA_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("znaide_qc_u4_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ZNAIDE_DATA_DIR", &dir);
        let mut cfg = Config::default();
        cfg.save_protocol("ollama", ProtocolKind::Chat).unwrap();
        assert_eq!(Config::load().unwrap().providers["ollama"].protocol, None);
        cfg.save_protocol("ollama", ProtocolKind::Response).unwrap();
        assert_eq!(
            Config::load().unwrap().providers["ollama"].protocol,
            Some(ProtocolKind::Response)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// need03 U5:save_api_key(None→不动;Some("")→清空;Some(k)→明文)
    #[test]
    fn quick_save_api_key_semantics() {
        let _g = crate::test_util::DATA_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("znaide_qc_u5_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ZNAIDE_DATA_DIR", &dir);
        let mut cfg = Config::default();
        cfg.save_api_key("myco", Some("sk-abc")).unwrap();
        assert_eq!(
            Config::load().unwrap().providers["myco"].api_key.as_deref(),
            Some("sk-abc")
        );
        // None=不动(env 家守卫)
        cfg.save_api_key("myco", None).unwrap();
        assert_eq!(
            Config::load().unwrap().providers["myco"].api_key.as_deref(),
            Some("sk-abc")
        );
        // 空串=清空明文
        cfg.save_api_key("myco", Some("")).unwrap();
        assert_eq!(Config::load().unwrap().providers["myco"].api_key, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// need03 U6:switch_provider 换指针不丢别家条目;不存在的家建空条目不 panic
    #[test]
    fn quick_switch_provider_keeps_other_entries() {
        let _g = crate::test_util::DATA_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("znaide_qc_u6_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ZNAIDE_DATA_DIR", &dir);
        let mut cfg = Config::default();
        cfg.save_model("houseA", "model-a").unwrap();
        cfg.switch_provider("brand-new-house").unwrap();
        let back = Config::load().unwrap();
        assert_eq!(back.provider.as_deref(), Some("brand-new-house"));
        assert_eq!(
            back.providers["houseA"].model.as_deref(),
            Some("model-a")
        );
        assert!(back.providers.contains_key("brand-new-house"));
        // save_base_url 非法端点拒绝落盘
        assert!(cfg.save_base_url("x", "ftp://bad").is_err());
        cfg.save_base_url("houseA", "https://api.example.com/v1").unwrap();
        assert_eq!(
            Config::load().unwrap().providers["houseA"].base_url.as_deref(),
            Some("https://api.example.com/v1")
        );
        // save_session_header 只动目标家
        cfg.save_session_header("houseA", true).unwrap();
        let back = Config::load().unwrap();
        assert!(back.providers["houseA"].session_header);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// need03 R2 守卫:按需 save_* 不得清空顶层三件套(与全量 save() 口径隔离)
    #[test]
    fn quick_save_star_keeps_top_level_overrides() {
        let _g = crate::test_util::DATA_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("znaide_qc_r2_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ZNAIDE_DATA_DIR", &dir);
        let mut cfg = Config::default();
        cfg.model = Some("top-override".into());
        cfg.persist().unwrap();
        cfg.save_max_turns(Some(7)).unwrap();
        let back = Config::load().unwrap();
        assert_eq!(back.model.as_deref(), Some("top-override"));
        assert_eq!(back.max_turns, Some(7));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 代理 env 隔离：清掉全部代理相关 env（大小写全套 + ZNAIDE_*）。
    fn clear_proxy_env() {
        for v in [
            "ZNAIDE_PROXY",
            "ZNAIDE_NO_PROXY",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
            "HTTP_PROXY",
            "http_proxy",
            "NO_PROXY",
            "no_proxy",
        ] {
            std::env::remove_var(v);
        }
    }

    /// need03 U1:ProxyMode::parse 命中/回环；非法 → None
    #[test]
    fn proxy_mode_parse_roundtrip() {
        assert_eq!(ProxyMode::parse("auto"), Some(ProxyMode::Auto));
        assert_eq!(ProxyMode::parse("env"), Some(ProxyMode::Auto));
        assert_eq!(ProxyMode::parse("follow"), Some(ProxyMode::Auto));
        assert_eq!(ProxyMode::parse("OFF"), Some(ProxyMode::Off));
        assert_eq!(ProxyMode::parse("direct"), Some(ProxyMode::Off));
        assert_eq!(ProxyMode::parse("直连"), Some(ProxyMode::Off));
        assert_eq!(ProxyMode::parse("manual"), Some(ProxyMode::Manual));
        assert_eq!(ProxyMode::parse("手动"), Some(ProxyMode::Manual));
        assert_eq!(ProxyMode::parse("foo"), None);
        assert_eq!(ProxyMode::parse(""), None);
        assert_eq!(ProxyMode::Auto.as_str(), "auto");
        assert_eq!(ProxyMode::Off.as_str(), "off");
        assert_eq!(ProxyMode::Manual.as_str(), "manual");
        assert_eq!(ProxyConfig::default().mode, ProxyMode::Auto);
    }

    /// need03 U2:normalize_proxy_url 形态/归一/报错
    #[test]
    fn proxy_url_normalize_and_reject() {
        // 过：四种 scheme + 认证串保留 + 尾斜杠去掉
        assert_eq!(
            normalize_proxy_url("http://127.0.0.1:10808").unwrap(),
            "http://127.0.0.1:10808"
        );
        assert_eq!(
            normalize_proxy_url("http://127.0.0.1:10808/").unwrap(),
            "http://127.0.0.1:10808"
        );
        assert_eq!(
            normalize_proxy_url("HTTP://127.0.0.1:10808").unwrap(),
            "http://127.0.0.1:10808"
        );
        assert_eq!(
            normalize_proxy_url("socks5h://127.0.0.1:1080").unwrap(),
            "socks5h://127.0.0.1:1080"
        );
        assert_eq!(
            normalize_proxy_url("http://u:p@host:8080").unwrap(),
            "http://u:p@host:8080"
        );
        assert!(valid_proxy_url("https://proxy.example.com:3128"));
        // 拦：ftp scheme / 无 host / 空串 / 无 scheme
        assert!(normalize_proxy_url("ftp://x:21").is_err());
        assert!(normalize_proxy_url("http:///").is_err());
        assert!(normalize_proxy_url("").is_err());
        assert!(normalize_proxy_url("127.0.0.1:10808").is_err());
        assert!(!valid_proxy_url("ftp://x:21"));
    }

    /// need03 U3:save_proxy 落盘（Manual 存 url；切 Auto/Off 清残留；非法拒绝落盘且文件未动）
    #[test]
    fn quick_save_proxy_roundtrip() {
        let _g = crate::test_util::DATA_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("znaide_qc_px_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ZNAIDE_DATA_DIR", &dir);
        let mut cfg = Config::default();
        cfg.save_proxy(ProxyConfig {
            mode: ProxyMode::Manual,
            url: Some("http://127.0.0.1:10808/".into()),
        })
        .unwrap();
        let back = Config::load().unwrap();
        assert_eq!(back.proxy.mode, ProxyMode::Manual);
        // 归一：尾斜杠去掉
        assert_eq!(back.proxy.url.as_deref(), Some("http://127.0.0.1:10808"));
        // 切 Auto 清掉残留 url
        cfg = back;
        cfg.save_proxy(ProxyConfig {
            mode: ProxyMode::Auto,
            url: Some("http://127.0.0.1:10808".into()),
        })
        .unwrap();
        let back = Config::load().unwrap();
        assert_eq!(back.proxy.mode, ProxyMode::Auto);
        assert_eq!(back.proxy.url, None);
        // 非法 url 拒绝落盘（Err 且文件未动）
        assert!(cfg
            .save_proxy(ProxyConfig {
                mode: ProxyMode::Manual,
                url: Some("ftp://bad".into()),
            })
            .is_err());
        let back = Config::load().unwrap();
        assert_eq!(back.proxy.mode, ProxyMode::Auto);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// need03 U4:非法 mode 回退（{"mode":"foo"} → Auto，不炸整份 config）
    #[test]
    fn proxy_illegal_mode_falls_back_to_auto() {
        let cfg: Config = serde_json::from_str(
            r#"{"provider":"ollama","proxy":{"mode":"foo","url":"http://127.0.0.1:1"}}"#,
        )
        .unwrap();
        assert_eq!(cfg.proxy.mode, ProxyMode::Auto);
        // 缺键 → Auto（老配置零迁移）
        let cfg: Config = serde_json::from_str(r#"{"provider":"ollama"}"#).unwrap();
        assert_eq!(cfg.proxy.mode, ProxyMode::Auto);
        assert_eq!(cfg.proxy.url, None);
    }

    /// need03 U5:resolve_proxy 优先级
    /// CLI --proxy/--no-proxy > ZNAIDE_NO_PROXY > 文件 Off > ZNAIDE_PROXY > 文件 Manual
    /// > 环境(HTTPS_PROXY…) > 直连
    #[test]
    fn proxy_resolve_priority() {
        let _g = crate::test_util::DATA_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        clear_proxy_env();
        let manual = Config {
            proxy: ProxyConfig {
                mode: ProxyMode::Manual,
                url: Some("http://127.0.0.1:10808".into()),
            },
            ..Default::default()
        };
        // 全空 → Direct
        assert_eq!(
            Config::default().resolve_proxy(None, false),
            EffectiveProxy::Direct
        );
        // 文件 Manual 生效
        assert_eq!(
            manual.resolve_proxy(None, false),
            EffectiveProxy::Via("http://127.0.0.1:10808".into())
        );
        // 环境 HTTPS_PROXY 生效（Auto 跟随）
        std::env::set_var("HTTPS_PROXY", "http://127.0.0.1:18080");
        assert_eq!(
            Config::default().resolve_proxy(None, false),
            EffectiveProxy::Via("http://127.0.0.1:18080".into())
        );
        // 文件 Manual 优先于 HTTPS_PROXY（只有 ZNAIDE_PROXY 能覆盖文件 manual）
        assert_eq!(
            manual.resolve_proxy(None, false),
            EffectiveProxy::Via("http://127.0.0.1:10808".into())
        );
        // ZNAIDE_PROXY 覆盖文件 manual
        std::env::set_var("ZNAIDE_PROXY", "http://127.0.0.1:19090");
        assert_eq!(
            manual.resolve_proxy(None, false),
            EffectiveProxy::Via("http://127.0.0.1:19090".into())
        );
        // ZNAIDE_NO_PROXY 掐直连
        std::env::set_var("ZNAIDE_NO_PROXY", "1");
        assert_eq!(manual.resolve_proxy(None, false), EffectiveProxy::Direct);
        // CLI --proxy 覆盖 ENV
        assert_eq!(
            manual.resolve_proxy(Some("http://127.0.0.1:17070".into()), false),
            EffectiveProxy::Via("http://127.0.0.1:17070".into())
        );
        // --no-proxy 最高
        assert_eq!(
            manual.resolve_proxy(Some("http://127.0.0.1:17070".into()), true),
            EffectiveProxy::Direct
        );
        clear_proxy_env();
        // 文件 Off 时 env 有代理仍直连
        std::env::set_var("HTTPS_PROXY", "http://127.0.0.1:18080");
        let off = Config {
            proxy: ProxyConfig {
                mode: ProxyMode::Off,
                url: None,
            },
            ..Default::default()
        };
        assert_eq!(off.resolve_proxy(None, false), EffectiveProxy::Direct);
        clear_proxy_env();
    }

    /// need03 U6:EffectiveProxy::display 脱敏
    #[test]
    fn proxy_display_masks_password() {
        assert_eq!(
            EffectiveProxy::Via("http://u:p@h:8080".into()).display(),
            "http://h:8080"
        );
        assert_eq!(
            EffectiveProxy::Via("socks5h://h:1080".into()).display(),
            "socks5h://h:1080"
        );
        assert_eq!(EffectiveProxy::Direct.display(), "直连");
        assert_eq!(
            EffectiveProxy::Via("http://h:8080".into()).url(),
            Some("http://h:8080")
        );
        assert_eq!(EffectiveProxy::Direct.url(), None);
    }

    /// need03 U7:quick_save_proxy 不清空顶层 model（R2 守卫同构）
    #[test]
    fn quick_save_proxy_keeps_top_level() {
        let _g = crate::test_util::DATA_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("znaide_qc_px2_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ZNAIDE_DATA_DIR", &dir);
        let mut cfg = Config {
            model: Some("top-override".into()),
            ..Default::default()
        };
        cfg.persist().unwrap();
        cfg.save_proxy(ProxyConfig {
            mode: ProxyMode::Off,
            url: None,
        })
        .unwrap();
        let back = Config::load().unwrap();
        assert_eq!(back.model.as_deref(), Some("top-override"));
        assert_eq!(back.proxy.mode, ProxyMode::Off);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
