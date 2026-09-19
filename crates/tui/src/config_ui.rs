//! 配置向导状态机(服务商 → 模型 → 协议 → Key → 上下文窗口 → 轮数上限 → 验证)。
//! 与 app.rs 解耦:on_key 返回 WizardAction,宿主执行异步动作(查询模型/验证),
//! 再把结果经 inject_models / inject_verify 回填。
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use znaide_core::config::Config;
use znaide_core::config::ProtocolKind;
use znaide_core::config::ProviderDef;
use znaide_core::config::ProxyConfig;
use znaide_core::config::ProxyMode;
use znaide_core::config::Resolved;
use znaide_core::config::RetryConfig;

/// 向导步骤
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// 选择服务商(列表)
    Provider,
    /// 是否为该服务商启用稳定会话头(x-opencode-session，同一会话内稳定，用于服务端缓存)
    SessionHeader,
    /// 正在查询模型(等待宿主回调)
    Querying,
    /// 模型列表(可上下选,或手动输入)
    ModelSelect,
    /// 协议类型(chat/response,默认 chat)
    ProtocolSelect,
    /// 输入 API Key
    ApiKey,
    /// 上下文窗口 token 数(状态栏 ctx 占用条的分母;留空 = 按模型名自动识别)
    ContextWindow,
    /// 轮数上限(单条消息最多几轮模型往返;0 = 不限)
    MaxTurns,
    /// 弱网重试(单次模型调用遇空回复/可重试错误后的追加次数;关闭/3/5/8/手动)
    Retry,
    /// 全局网络代理(跟随环境/直连/手动指定；模型请求/抓取/更新统一出口)
    Proxy,
    /// 正在验证(等待宿主回调)
    Verifying,
}

/// 模型列表一次最多画多少行;超出的部分靠滚动窗口查看
/// (固定 take(N) 会让游标移出屏幕,回车选中看不见的模型)
const MODEL_WINDOW: usize = 25;

/// 宿主需要执行的异步动作
pub enum WizardAction {
    None,
    /// 查询模型列表(参数:base_url + api_key + 会话头探针值,由宿主调用 OpenAiClient)。
    /// 开关关 → session_header 为 None(不发头)。
    FetchModels {
        base_url: String,
        api_key: Option<String>,
        session_header: Option<String>,
        proxy: znaide_core::config::EffectiveProxy,
    },
    /// 验证连接(base_url/model/api_key,由宿主调用 client.validate)
    Verify(Resolved),
    /// 退出向导
    Exit,
}

pub struct SetupWizard {
    pub step: Step,
    /// 可用服务商(名称 + 端点,供选择)
    pub providers: Vec<(String, String, String)>,
    /// 游标(provider 列表 / 模型列表)
    pub cursor: usize,
    // ---- 已收集的配置 ----
    pub provider: String,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    /// 自动查询到的模型
    pub models: Vec<String>,
    /// 模型选择游标
    pub model_cursor: usize,
    /// 手动输入模式(模型名/自定义端点)
    pub typing: bool,
    pub input_buf: String,
    /// 提示/错误信息
    pub notice: String,
    /// 查询/验证中的进度文案
    pub progress: String,
    /// 单条消息轮数上限(None = 用默认;Some(0) = 不限)。由宿主在打开面板时
    /// 用 `prefill()` 从现有配置预填,保存时随其他字段一起落盘。
    pub max_turns: Option<usize>,
    /// 上下文窗口 token 数(None = 按模型名查内置表)。同样由 `prefill()` 带出、
    /// 保存时写进当前 provider 条目;模型不在内置表里时就靠这里填准(否则 ctx 条
    /// 只报绝对量、不给百分比)。
    pub context_window: Option<usize>,
    /// 协议类型(默认 chat)。由 `prefill()` 带出、保存时写进当前 provider 条目。
    pub protocol: ProtocolKind,
    /// 协议选择游标(0=chat, 1=response)
    pub protocol_cursor: usize,
    /// 是否为当前服务商启用稳定会话头(默认关)。由 `prefill()`/切服务商时按家带出、
    /// 保存时写进当前 provider 条目;同一会话内所有模型请求携带同一稳定值(会话 ID)。
    pub session_header: bool,
    /// 会话头选择游标(0=是, 1=否，默认 1=否)
    pub session_cursor: usize,
    /// 当前 typing 输入的是 max_turns(与模型名/key/端点共用输入通道,靠它区分)
    pub typing_max_turns: bool,
    /// 当前 typing 输入的是 context_window
    pub typing_context_window: bool,
    /// 弱网重试策略(默认关闭)。由 `prefill()` 带出、保存时写进顶层 retry；
    /// draft 进 Resolved，随重配即时生效。
    pub retry: RetryConfig,
    /// 重试档位游标(0=关闭, 1=3次, 2=5次, 3=8次, 4=手动输入)
    pub retry_cursor: usize,
    /// 当前 typing 输入的是 retry 手动次数
    pub typing_retry: bool,
    /// 全局网络代理三档原文（默认 Auto）。由 `prefill()` 带出、保存时写进顶层 proxy；
    /// draft 落定进 Resolved，随重配即时生效。切家不重置（全局，与家无关）。
    pub proxy: ProxyConfig,
    /// 代理档位游标(0=跟随环境, 1=直连, 2=手动指定)
    pub proxy_cursor: usize,
    /// 当前 typing 输入的是 proxy 手动地址
    pub typing_proxy: bool,
    /// Key 来自环境变量(api_key_env):面板里显示为空但实际可用
    pub key_from_env: bool,
    /// 打开面板时快照的合并后 provider 表:切服务商时按名字取"它自己"的
    /// 模型/端点/Key(避免沿用上一家的模型或把上一家的 Key 带过去)
    pub provider_defs: std::collections::HashMap<String, ProviderDef>,
}

impl SetupWizard {
    pub fn new() -> Self {
        let providers = provider_list();
        Self {
            step: Step::Provider,
            providers,
            cursor: 0,
            provider: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            models: Vec::new(),
            model_cursor: 0,
            typing: false,
            input_buf: String::new(),
            notice: String::new(),
            progress: String::new(),
            max_turns: None,
            context_window: None,
            protocol: ProtocolKind::default(),
            protocol_cursor: 0,
            session_header: false,
            session_cursor: 1,
            typing_max_turns: false,
            typing_context_window: false,
            retry: RetryConfig::disabled(),
            retry_cursor: 0,
            typing_retry: false,
            proxy: ProxyConfig::default(),
            proxy_cursor: 0,
            typing_proxy: false,
            key_from_env: false,
            provider_defs: std::collections::HashMap::new(),
        }
    }

    /// 切到某个服务商时,带上**它自己**的模型与 Key。
    /// Key 取值顺序:该条目明文 > 该条目的 api_key_env 环境变量 > 空(请用户输入)。
    /// 这条修的是"配好 A 再切回 B,面板里还是 A 的模型/Key"的错配。
    fn apply_provider_defaults(&mut self, name: &str) {
        let def = self.provider_defs.get(name).cloned();
        self.model = def
            .as_ref()
            .and_then(|d| d.model.clone())
            .unwrap_or_default();
        let plain = def
            .as_ref()
            .and_then(|d| d.api_key.clone())
            .filter(|k| !k.is_empty());
        // 环境变量名也取自同一份快照(别再读盘,免得和 provider_defs 不一致)
        let from_env = def
            .as_ref()
            .and_then(|d| d.api_key_env.clone())
            .and_then(|e| std::env::var(e).ok())
            .filter(|v| !v.is_empty());
        self.api_key = plain.or(from_env).unwrap_or_default();
        // 这家没有明文 Key、而是靠 api_key_env:标记出来(界面提示 + 保存时别把
        // 环境变量里的 Key 落成明文)
        let plain_in_entry = def
            .as_ref()
            .and_then(|d| d.api_key.clone())
            .filter(|k| !k.is_empty())
            .is_some();
        self.key_from_env = !plain_in_entry
            && def
                .as_ref()
                .map(|d| d.api_key_env.is_some())
                .unwrap_or(false);
        self.model_cursor = 0;
        self.models.clear();
        // 窗口也是"这家自己的":切换时不能沿用上一家的(40k 的 ollama 与 1M 的云端来回切会算错)
        self.context_window = def.as_ref().and_then(|d| d.context_window);
        // 协议同样跟服务商走(避免沿用上一家的协议)
        self.protocol = def.as_ref().and_then(|d| d.protocol).unwrap_or_default();
        self.protocol_cursor = match self.protocol {
            ProtocolKind::Chat => 0,
            ProtocolKind::Response => 1,
        };
        // 会话头开关同样跟服务商走:换家即换开关,不沿用上一家
        self.session_header = def.as_ref().map(|d| d.session_header).unwrap_or(false);
        self.session_cursor = if self.session_header { 0 } else { 1 };
        // 代理是全局网络环境配置，不跟家走：切家不重置（与 retry 的顶层口径一致，
        // 但 retry 同样不跟家走——这里刻意不碰 self.proxy，防后人“顺手”加上）
    }

    /// 用现有配置预填(重开 `/config` 时把已经配好的值显示出来)。
    /// 不在 `new()` 里读盘,免得测试受本机 ~/.znaide/config.json 影响。
    pub fn prefill(&mut self) {
        if let Ok(cfg) = Config::load() {
            self.apply_config(&cfg);
        }
    }

    /// 预填的纯逻辑(便于测试):服务商/模型/端点/Key/轮数上限全部带出,
    /// 游标落在当前服务商上,并在顶部显示"当前配置"摘要。
    pub fn apply_config(&mut self, cfg: &Config) {
        let r = cfg.resolve(None, None, None, None, None, None).ok();
        // 服务商:config 的 provider,兜底解析出的名字
        let provider = cfg
            .provider
            .clone()
            .filter(|p| !p.is_empty())
            .or_else(|| r.as_ref().map(|x| x.provider_name.clone()));
        if let Some(p) = provider {
            self.provider = p.clone();
            // 游标落在当前服务商上,列表里一眼看到"现在用的是哪个"
            if let Some(i) = self.providers.iter().position(|(n, _, _)| *n == p) {
                self.cursor = i;
            }
        }
        if let Some(x) = &r {
            self.model = x.model.clone();
            self.base_url = x.base_url.clone();
        }
        // Key:顶层手动覆盖 > 当前 provider 条目里的明文。走 api_key_env 的
        // **不**预填成明文,否则验证通过保存时会把 key 落到文件里,违背用环境变量的初衷。
        let def = cfg.all_providers().get(&self.provider).cloned();
        // 快照一份 provider 表:切服务商时按名字取"它自己"的模型/Key
        self.provider_defs = cfg.all_providers();
        let top_plain = cfg.api_key.clone().filter(|k| !k.is_empty());
        let entry_plain = def
            .as_ref()
            .and_then(|d| d.api_key.clone())
            .filter(|k| !k.is_empty());
        self.api_key = top_plain
            .clone()
            .or(entry_plain.clone())
            .unwrap_or_default();
        // 文件里没有明文、这家靠 api_key_env → 标记(界面提示,且保存时不写明文)
        self.key_from_env = top_plain.is_none()
            && entry_plain.is_none()
            && def
                .as_ref()
                .map(|d| d.api_key_env.is_some())
                .unwrap_or(false);
        self.max_turns = cfg.max_turns;
        // 窗口:当前 provider 条目里的固定值(没写 = None,界面显示"按模型名自动")
        self.context_window = def.as_ref().and_then(|d| d.context_window);
        // 协议:顶层手动覆盖优先(与 model 的口径一致),否则条目值,否则 chat
        self.protocol = cfg
            .protocol
            .or_else(|| def.as_ref().and_then(|d| d.protocol))
            .unwrap_or_default();
        self.protocol_cursor = match self.protocol {
            ProtocolKind::Chat => 0,
            ProtocolKind::Response => 1,
        };
        // 会话头:当前 provider 条目值(缺键 = 关)。顶层无该开关，不参与覆盖。
        self.session_header = def.as_ref().map(|d| d.session_header).unwrap_or(false);
        self.session_cursor = if self.session_header { 0 } else { 1 };
        // 弱网重试:顶层 retry(缺键 = 关闭)。游标落在当前档位上。
        let mut retry = cfg.retry;
        retry.max_retries = retry.max_retries.min(znaide_core::config::MAX_RETRIES);
        self.retry = retry;
        self.retry_cursor = if !retry.enabled {
            0
        } else {
            match retry.max_retries {
                3 => 1,
                5 => 2,
                8 => 3,
                _ => 4,
            }
        };
        // 代理全局（顶层 proxy，缺键 = Auto）。归一：非 manual 清掉残留 url。
        let mut proxy = cfg.proxy.clone();
        if proxy.mode != ProxyMode::Manual {
            proxy.url = None;
        }
        self.proxy = proxy;
        self.proxy_cursor = match self.proxy.mode {
            ProxyMode::Auto => 0,
            ProxyMode::Off => 1,
            ProxyMode::Manual => 2,
        };
    }

    /// 是否已有配置可展示(首次运行全空 → 不显示摘要行)
    pub fn has_configured(&self) -> bool {
        !self.provider.is_empty() || !self.model.is_empty() || !self.api_key.is_empty()
    }

    /// 轮数上限的展示文案
    pub fn max_turns_label(&self) -> String {
        max_turns_text(self.max_turns)
    }

    /// 弱网重试的展示文案
    pub fn retry_label(&self) -> String {
        retry_text(&self.retry)
    }

    /// 上下文窗口的展示文案:自动 = 按模型名查内置表(查不到就只说绝对量)
    pub fn context_window_label(&self) -> String {
        context_window_text(self.context_window)
    }

    /// 模型列表的滚动窗口:返回(起点下标, 画几行),保证 `model_cursor` 总在窗口内
    pub fn model_window(&self) -> (usize, usize) {
        let n = self.models.len();
        if n <= MODEL_WINDOW {
            return (0, n);
        }
        let first = self
            .model_cursor
            .saturating_sub(MODEL_WINDOW / 2)
            .min(n - MODEL_WINDOW);
        (first, MODEL_WINDOW)
    }

    /// 当前形成的 Resolved(未验证)
    pub fn draft(&self) -> Resolved {
        Resolved {
            provider_name: self.provider.clone(),
            model: self.model.clone(),
            base_url: self.base_url.clone(),
            api_key: if self.api_key.is_empty() {
                None
            } else {
                Some(self.api_key.clone())
            },
            // 面板里填的固定窗口(空 = 交给内置表/未知);验证不需要它,但保存要
            context_window: self.context_window,
            protocol: self.protocol,
            session_header_enabled: self.session_header,
            retry: self.retry,
            proxy: self.effective_proxy_for_draft(),
        }
    }

    /// 面板当前三档按 03 §3 落定（env 部分照读，CLI 无；与 resolve_proxy 同优先级，别手写第二份）。
    pub fn effective_proxy_for_draft(&self) -> znaide_core::config::EffectiveProxy {
        match self.proxy.mode {
            ProxyMode::Off => znaide_core::config::EffectiveProxy::Direct,
            ProxyMode::Manual => {
                match self
                    .proxy
                    .url
                    .as_deref()
                    .map(str::trim)
                    .filter(|u| !u.is_empty())
                {
                    Some(u) => znaide_core::config::EffectiveProxy::Via(u.to_string()),
                    // 面板 confirm 已拦空串；此处兜底按跟随环境
                    None => match znaide_core::update::env_proxy_url() {
                        Some(e) => znaide_core::config::EffectiveProxy::Via(e),
                        None => znaide_core::config::EffectiveProxy::Direct,
                    },
                }
            }
            ProxyMode::Auto => match znaide_core::update::env_proxy_url() {
                Some(e) => znaide_core::config::EffectiveProxy::Via(e),
                None => znaide_core::config::EffectiveProxy::Direct,
            },
        }
    }

    /// 进入代理选择:光标落在当前档位上(跟随环境/直连/手动指定)
    fn enter_proxy(&mut self) {
        self.proxy_cursor = match self.proxy.mode {
            ProxyMode::Auto => 0,
            ProxyMode::Off => 1,
            ProxyMode::Manual => 2,
        };
        self.step = Step::Proxy;
    }

    /// 档位游标变化即时落到 proxy(手动档保持原 url，进输入后才改)
    fn apply_proxy_cursor(&mut self) {
        self.proxy = proxy_from_cursor(self.proxy_cursor, self.proxy.url.clone());
    }

    /// 进入会话头选择:光标落在当前值上(重开面板预填时落在已存值上)
    fn enter_session_header(&mut self) {
        self.session_cursor = if self.session_header { 0 } else { 1 };
        self.step = Step::SessionHeader;
    }

    /// 进入协议选择:光标落在当前值上(重开面板预填时落在已存值上)
    fn enter_protocol_select(&mut self) {
        self.protocol_cursor = match self.protocol {
            ProtocolKind::Chat => 0,
            ProtocolKind::Response => 1,
        };
        self.step = Step::ProtocolSelect;
    }

    /// 进入弱网重试:光标落在当前档位上(关闭/3/5/8/手动)
    fn enter_retry(&mut self) {
        self.retry_cursor = if !self.retry.enabled {
            0
        } else {
            match self.retry.max_retries {
                3 => 1,
                5 => 2,
                8 => 3,
                _ => 4,
            }
        };
        self.step = Step::Retry;
    }

    /// 档位游标变化即时落到 retry(手动档保持原值，进输入后才改)
    fn apply_retry_cursor(&mut self) {
        match self.retry_cursor {
            0 => self.retry = RetryConfig::disabled(),
            1 => self.retry = RetryConfig::enabled_with(3),
            2 => self.retry = RetryConfig::enabled_with(5),
            3 => self.retry = RetryConfig::enabled_with(8),
            _ => {}
        }
    }

    /// 宿主注入模型查询结果
    pub fn inject_models(&mut self, result: Result<Vec<String>, String>) {
        self.step = Step::ModelSelect;
        self.typing = false;
        match result {
            Ok(models) if !models.is_empty() => {
                let mut models = models;
                // 当前模型不在服务商给出的列表里(如自定模型名)→ 保留它并置顶选中,
                // 免得用户只是"切回这家",回车一下模型就被悄悄换成列表第一项
                if !self.model.is_empty() && !models.iter().any(|m| *m == self.model) {
                    models.insert(0, self.model.clone());
                }
                let n = models.len();
                self.models = models;
                self.notice = format!("找到 {n} 个模型:↑↓ 选择,m 手动输入");
                // 之前预设的模型还在列表里就直接选中
                if let Some(i) = self.models.iter().position(|m| *m == self.model) {
                    self.model_cursor = i;
                }
            }
            Ok(_) => {
                self.models.clear();
                self.notice = "该服务商没返回模型列表(可能不支持 /models),直接输入模型名:".into();
                self.typing = true;
                // 带出当前模型:不改就直接回车沿用
                self.input_buf = self.model.clone();
            }
            Err(e) => {
                self.models.clear();
                self.notice = format!("模型列表查询失败: {e}\n直接输入模型名(回车继续):");
                self.typing = true;
                self.input_buf = self.model.clone();
            }
        }
    }

    /// 宿主注入验证结果
    pub fn inject_verify(&mut self, ok: bool, msg: String) {
        self.step = Step::ApiKey;
        if ok {
            self.notice = format!("✔ 验证通过:{msg}");
        } else {
            self.notice = format!("✘ 验证失败:{msg}\n改完按 s 重新验证,或 Esc 退出检查");
        }
    }

    /// 进入"正在查询模型"状态(宿主开始查询前调用)。
    /// 开关开时带上一次性探针头值(验证阶段尚无正式 Session，仅走通缓存路由)。
    fn start_querying(&mut self, base_url: &str, api_key: Option<&str>) -> WizardAction {
        self.step = Step::Querying;
        self.progress = format!("正在查询 {base_url} 的模型列表…");
        self.base_url = base_url.to_string();
        self.api_key = api_key.unwrap_or("").to_string();
        WizardAction::FetchModels {
            base_url: base_url.to_string(),
            api_key: api_key.map(|s| s.to_string()),
            session_header: if self.session_header {
                Some(znaide_core::llm::openai::probe_session_value())
            } else {
                None
            },
            // 验证阶段的模型列表查询同样走待存代理
            proxy: self.effective_proxy_for_draft(),
        }
    }

    /// 文本输入态收到粘贴:把内容追加进输入缓冲,**只填充、不提交**
    /// (提交仍按回车),换行一律丢掉——Key / 端点 / 模型名 / 轮数都是单行。
    /// 调用方负责先做终端控制字符消毒(避免鼠标残片混入)。返回是否接收。
    pub fn paste_text(&mut self, text: &str) -> bool {
        if !self.typing {
            return false;
        }
        self.input_buf
            .extend(text.chars().filter(|c| *c != '\n' && *c != '\r'));
        true
    }

    pub fn on_key(&mut self, key: KeyEvent) -> WizardAction {
        // 手动输入模式(模型名 / 自定义端点 / key 输入共用一个输入逻辑)
        if self.typing {
            match key.code {
                KeyCode::Enter => {
                    let v = std::mem::take(&mut self.input_buf).trim().to_string();
                    self.typing = false;
                    return self.confirm_typed(&v);
                }
                KeyCode::Esc => {
                    self.typing = false;
                    self.typing_max_turns = false;
                    self.typing_context_window = false;
                    self.typing_retry = false;
                    self.typing_proxy = false;
                    self.input_buf.clear();
                }
                KeyCode::Backspace => {
                    self.input_buf.pop();
                }
                KeyCode::Char(c) => self.input_buf.push(c),
                _ => {}
            }
            return WizardAction::None;
        }

        // 向导步骤(同步渲染状态)
        match self.step {
            Step::Provider => match key.code {
                KeyCode::Esc => WizardAction::Exit,
                KeyCode::Up | KeyCode::Char('k') => {
                    self.cursor = self.cursor.saturating_sub(1);
                    WizardAction::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.cursor = (self.cursor + 1).min(self.providers.len().saturating_sub(1));
                    WizardAction::None
                }
                KeyCode::Enter | KeyCode::Char('e') => {
                    let (name, base, _default_model) =
                        self.providers[self.cursor.min(self.providers.len() - 1)].clone();
                    // 重开面板时选中的就是当前服务商:保留已预填的 key,
                    // 别把用户配好的 key 清掉(只有真的换了服务商才重新取)
                    let switching = name != self.provider;
                    self.provider = name.clone();
                    if switching {
                        // 切到哪家就带哪家的模型/Key(关键:别沿用上一家的)
                        self.apply_provider_defaults(&name);
                    }
                    if name == "custom" {
                        // 自定义服务商:先输入端点
                        self.typing = true;
                        self.input_buf.clear();
                        self.notice = "输入自定义端点(base_url),例如 http://localhost:11434/v1 或 https://api.example.com/v1:".into();
                        return WizardAction::None;
                    }
                    // 选完供应商 → 先问是否启用会话头(确认后才查询模型，
                    // 查询时即可按开关带探针头)。端点先记下，确认后再用。
                    self.base_url = base;
                    self.enter_session_header();
                    WizardAction::None
                }
                _ => WizardAction::None,
            },
            Step::SessionHeader => match key.code {
                KeyCode::Esc => {
                    self.step = Step::Provider;
                    WizardAction::None
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.session_cursor = self.session_cursor.saturating_sub(1);
                    self.session_header = self.session_cursor == 0;
                    WizardAction::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.session_cursor = (self.session_cursor + 1).min(1);
                    self.session_header = self.session_cursor == 0;
                    WizardAction::None
                }
                // Y/y 直达是，N/n 直达否
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.session_cursor = 0;
                    self.session_header = true;
                    let base = self.base_url.clone();
                    let key = if self.api_key.is_empty() {
                        None
                    } else {
                        Some(self.api_key.clone())
                    };
                    self.start_querying(&base, key.as_deref())
                }
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    self.session_cursor = 1;
                    self.session_header = false;
                    let base = self.base_url.clone();
                    let key = if self.api_key.is_empty() {
                        None
                    } else {
                        Some(self.api_key.clone())
                    };
                    self.start_querying(&base, key.as_deref())
                }
                KeyCode::Enter | KeyCode::Char('e') => {
                    let base = self.base_url.clone();
                    let key = if self.api_key.is_empty() {
                        None
                    } else {
                        Some(self.api_key.clone())
                    };
                    // 尝试模型自动查询;失败可手输
                    self.start_querying(&base, key.as_deref())
                }
                _ => WizardAction::None,
            },
            Step::Querying => {
                // 等待宿主;按 Esc 放弃回到会话头选择
                if matches!(key.code, KeyCode::Esc) {
                    self.step = Step::SessionHeader;
                }
                WizardAction::None
            }
            Step::ModelSelect => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.model_cursor = self.model_cursor.saturating_sub(1);
                    WizardAction::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.model_cursor =
                        (self.model_cursor + 1).min(self.models.len().saturating_sub(1));
                    WizardAction::None
                }
                KeyCode::Enter | KeyCode::Char('e') => {
                    if !self.models.is_empty() {
                        self.model =
                            self.models[self.model_cursor.min(self.models.len() - 1)].clone();
                    }
                    self.enter_protocol_select();
                    WizardAction::None
                }
                KeyCode::Char('m') | KeyCode::Char('M') => {
                    // 手动输入模型
                    self.typing = true;
                    self.input_buf.clear();
                    WizardAction::None
                }
                KeyCode::Esc => {
                    self.step = Step::Provider;
                    WizardAction::None
                }
                _ => WizardAction::None,
            },
            Step::ProtocolSelect => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.protocol_cursor = self.protocol_cursor.saturating_sub(1);
                    self.protocol =
                        [ProtocolKind::Chat, ProtocolKind::Response][self.protocol_cursor.min(1)];
                    WizardAction::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.protocol_cursor = (self.protocol_cursor + 1).min(1);
                    self.protocol =
                        [ProtocolKind::Chat, ProtocolKind::Response][self.protocol_cursor];
                    WizardAction::None
                }
                KeyCode::Enter | KeyCode::Char('e') => {
                    self.step = Step::ApiKey;
                    WizardAction::None
                }
                KeyCode::Esc => {
                    self.step = Step::ModelSelect;
                    WizardAction::None
                }
                _ => WizardAction::None,
            },
            Step::ApiKey => match key.code {
                KeyCode::Esc => {
                    // 回退:直接回协议选择(保留模型);若没有模型就回服务商
                    self.step = if self.models.is_empty() {
                        Step::Provider
                    } else {
                        Step::ProtocolSelect
                    };
                    WizardAction::None
                }
                KeyCode::Char('s') | KeyCode::Char('S') => {
                    // 验证(跳过窗口/轮数上限那两步,直接用当前值)
                    self.step = Step::Verifying;
                    self.progress = format!("正在验证 {} / {} …", self.provider, self.model);
                    WizardAction::Verify(self.draft())
                }
                KeyCode::Enter => {
                    // 下一步:上下文窗口
                    self.step = Step::ContextWindow;
                    WizardAction::None
                }
                KeyCode::Char('e') | KeyCode::Char('E') => {
                    // 编辑 key(覆盖)
                    self.typing = true;
                    self.input_buf = std::mem::take(&mut self.api_key);
                    WizardAction::None
                }
                _ => WizardAction::None,
            },
            Step::ContextWindow => match key.code {
                // 直接敲数字就进输入(照着屏幕上显示的改最直观)
                KeyCode::Char(c) if c.is_ascii_digit() => {
                    self.typing = true;
                    self.typing_context_window = true;
                    self.input_buf.clear();
                    self.input_buf.push(c);
                    self.notice =
                        "输入当前模型的上下文窗口 token 数(留空 = 按模型名自动识别):".into();
                    WizardAction::None
                }
                KeyCode::Char('t')
                | KeyCode::Char('T')
                | KeyCode::Char('e')
                | KeyCode::Char('E') => {
                    self.typing = true;
                    self.typing_context_window = true;
                    self.input_buf = self
                        .context_window
                        .map(|v| v.to_string())
                        .unwrap_or_default();
                    self.notice =
                        "输入当前模型的上下文窗口 token 数(留空 = 按模型名自动识别):".into();
                    WizardAction::None
                }
                KeyCode::Enter => {
                    // 下一步:轮数上限
                    self.step = Step::MaxTurns;
                    WizardAction::None
                }
                KeyCode::Char('s') | KeyCode::Char('S') => {
                    // 跳过轮数上限那步,直接用当前值验证
                    self.step = Step::Verifying;
                    self.progress = format!("正在验证 {} / {} …", self.provider, self.model);
                    WizardAction::Verify(self.draft())
                }
                KeyCode::Esc => {
                    self.step = Step::ApiKey;
                    WizardAction::None
                }
                _ => WizardAction::None,
            },
            Step::MaxTurns => match key.code {
                // 直接敲数字就进输入(照着屏幕上显示的改最直观)
                KeyCode::Char(c) if c.is_ascii_digit() => {
                    self.typing = true;
                    self.typing_max_turns = true;
                    self.input_buf.clear();
                    self.input_buf.push(c);
                    self.notice =
                        "输入单条消息最多允许的模型往返轮数(0 = 不限,留空 = 默认):".into();
                    WizardAction::None
                }
                KeyCode::Char('t')
                | KeyCode::Char('T')
                | KeyCode::Char('e')
                | KeyCode::Char('E') => {
                    self.typing = true;
                    self.typing_max_turns = true;
                    self.input_buf = self.max_turns.map(|v| v.to_string()).unwrap_or_default();
                    self.notice =
                        "输入单条消息最多允许的模型往返轮数(0 = 不限,留空 = 默认):".into();
                    WizardAction::None
                }
                KeyCode::Enter | KeyCode::Char('s') | KeyCode::Char('S') => {
                    // 下一步:弱网重试
                    self.enter_retry();
                    WizardAction::None
                }
                KeyCode::Esc => {
                    self.step = Step::ContextWindow;
                    WizardAction::None
                }
                _ => WizardAction::None,
            },
            Step::Retry => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.retry_cursor = self.retry_cursor.saturating_sub(1);
                    self.apply_retry_cursor();
                    WizardAction::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.retry_cursor = (self.retry_cursor + 1).min(4);
                    self.apply_retry_cursor();
                    WizardAction::None
                }
                // 直接敲数字进手动输入(0..=8，超限夹取)
                KeyCode::Char(c) if c.is_ascii_digit() => {
                    self.typing = true;
                    self.typing_retry = true;
                    self.input_buf.clear();
                    self.input_buf.push(c);
                    self.notice =
                        "输入弱网重试次数(空/0 = 关闭，1..=8，超限按 8 算):".into();
                    WizardAction::None
                }
                KeyCode::Char('t')
                | KeyCode::Char('T')
                | KeyCode::Char('e')
                | KeyCode::Char('E') => {
                    self.typing = true;
                    self.typing_retry = true;
                    self.input_buf = if self.retry.enabled {
                        self.retry.max_retries.to_string()
                    } else {
                        String::new()
                    };
                    self.notice =
                        "输入弱网重试次数(空/0 = 关闭，1..=8，超限按 8 算):".into();
                    WizardAction::None
                }
                KeyCode::Enter => {
                    if self.retry_cursor >= 4 {
                        // 手动输入
                        self.typing = true;
                        self.typing_retry = true;
                        self.input_buf = if self.retry.enabled {
                            self.retry.max_retries.to_string()
                        } else {
                            String::new()
                        };
                        self.notice = "输入弱网重试次数(空/0 = 关闭，1..=8，超限按 8 算):".into();
                        WizardAction::None
                    } else {
                        // 下一步:网络代理
                        self.enter_proxy();
                        WizardAction::None
                    }
                }
                KeyCode::Char('s') | KeyCode::Char('S') => {
                    // 下一步:网络代理(用当前重试档位)
                    self.enter_proxy();
                    WizardAction::None
                }
                KeyCode::Esc => {
                    self.step = Step::MaxTurns;
                    WizardAction::None
                }
                _ => WizardAction::None,
            },
            Step::Proxy => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.proxy_cursor = self.proxy_cursor.saturating_sub(1);
                    self.apply_proxy_cursor();
                    WizardAction::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.proxy_cursor = (self.proxy_cursor + 1).min(2);
                    self.apply_proxy_cursor();
                    WizardAction::None
                }
                // y = 跟随环境，n = 直连
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.proxy_cursor = 0;
                    self.apply_proxy_cursor();
                    self.notice = "代理:跟随环境。按 s 验证并完成".into();
                    WizardAction::None
                }
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    self.proxy_cursor = 1;
                    self.apply_proxy_cursor();
                    self.notice = "代理:直连(忽略环境代理)。按 s 验证并完成".into();
                    WizardAction::None
                }
                // 直接敲数字进手动地址输入（照抄 Retry 数字口径；字母用 t/e/Enter 进输入再敲全串）
                KeyCode::Char(c) if c.is_ascii_digit() => {
                    self.typing = true;
                    self.typing_proxy = true;
                    self.input_buf.clear();
                    self.input_buf.push(c);
                    self.notice =
                        "输入代理地址(形如 http://127.0.0.1:10808，清空回车 = 跟随环境):".into();
                    WizardAction::None
                }
                KeyCode::Char('t')
                | KeyCode::Char('T')
                | KeyCode::Char('e')
                | KeyCode::Char('E')
                | KeyCode::Enter => {
                    self.typing = true;
                    self.typing_proxy = true;
                    self.input_buf = self.proxy.url.clone().unwrap_or_default();
                    self.notice =
                        "输入代理地址(形如 http://127.0.0.1:10808，清空回车 = 跟随环境):".into();
                    WizardAction::None
                }
                KeyCode::Char('s') | KeyCode::Char('S') => {
                    // 完成:验证(用当前代理档位)
                    self.step = Step::Verifying;
                    self.progress = format!("正在验证 {} / {} …", self.provider, self.model);
                    WizardAction::Verify(self.draft())
                }
                KeyCode::Esc => {
                    self.step = Step::Retry;
                    WizardAction::None
                }
                _ => WizardAction::None,
            },
            Step::Verifying => {
                // 等待宿主;按 Esc 回到上一步(网络代理)
                if matches!(key.code, KeyCode::Esc) {
                    self.step = Step::Proxy;
                }
                WizardAction::None
            }
        }
    }

    /// 处理输入框提交的内容(取决于当前阶段)
    fn confirm_typed(&mut self, v: &str) -> WizardAction {
        // 上下文窗口:与模型名/key/端点共用输入通道,靠 typing_context_window 区分
        if self.typing_context_window {
            self.typing_context_window = false;
            let t = v.trim();
            if t.is_empty() {
                self.context_window = None;
                self.notice = "上下文窗口:自动(按模型名匹配内置表)。按 s 验证并完成".into();
                return WizardAction::None;
            }
            return match t.parse::<usize>() {
                Ok(n) if n > 0 => {
                    self.context_window = Some(n);
                    self.notice = format!("上下文窗口:{n} tokens。按 s 验证并完成");
                    WizardAction::None
                }
                _ => {
                    // 回填原输入,让用户直接改错处
                    self.input_buf = t.to_string();
                    self.typing = true;
                    self.typing_context_window = true;
                    self.notice = "窗口要填正整数 tokens(留空 = 自动识别),请重新输入:".into();
                    WizardAction::None
                }
            };
        }
        // 轮数上限:与模型名/key/端点共用输入通道,靠 typing_max_turns 区分
        if self.typing_max_turns {
            self.typing_max_turns = false;
            let t = v.trim();
            if t.is_empty() {
                self.max_turns = None;
                self.notice = format!(
                    "轮数上限:随默认({} 轮)。按 s 验证并完成",
                    znaide_core::session::DEFAULT_MAX_TURNS
                );
                return WizardAction::None;
            }
            return match t.parse::<usize>() {
                Ok(0) => {
                    self.max_turns = Some(0);
                    self.notice = "轮数上限:不限(只靠重复调用刹车)。按 s 验证并完成".into();
                    WizardAction::None
                }
                Ok(n) => {
                    self.max_turns = Some(n);
                    self.notice = format!("轮数上限:{n} 轮。按 s 验证并完成");
                    WizardAction::None
                }
                Err(_) => {
                    // 回填原输入,让用户直接改错处
                    self.input_buf = t.to_string();
                    self.typing = true;
                    self.typing_max_turns = true;
                    self.notice = "轮数上限要填整数(0 = 不限,留空 = 默认),请重新输入:".into();
                    WizardAction::None
                }
            };
        }
        // 弱网重试次数:空/0 = 关闭，1..=8 开启(超限夹取 8)
        if self.typing_retry {
            self.typing_retry = false;
            let t = v.trim();
            if t.is_empty() {
                self.retry = RetryConfig::disabled();
                self.retry_cursor = 0;
                self.notice = "弱网重试:关闭。按 s 验证并完成".into();
                return WizardAction::None;
            }
            return match t.parse::<usize>() {
                Ok(0) => {
                    self.retry = RetryConfig::disabled();
                    self.retry_cursor = 0;
                    self.notice = "弱网重试:关闭。按 s 验证并完成".into();
                    WizardAction::None
                }
                Ok(n) => {
                    let n = n.min(znaide_core::config::MAX_RETRIES);
                    self.retry = RetryConfig::enabled_with(n);
                    self.retry_cursor = match n {
                        3 => 1,
                        5 => 2,
                        8 => 3,
                        _ => 4,
                    };
                    self.notice = format!("弱网重试:开启(失败后追加 {n} 次)。按 s 验证并完成");
                    WizardAction::None
                }
                Err(_) => {
                    self.input_buf = t.to_string();
                    self.typing = true;
                    self.typing_retry = true;
                    self.notice = "重试次数要填 0..=8 的整数(空 = 关闭)，请重新输入:".into();
                    WizardAction::None
                }
            };
        }
        // 代理手动地址:空 = 跟随环境；合法归一进 Manual；非法回填重输
        if self.typing_proxy {
            self.typing_proxy = false;
            let t = v.trim();
            if t.is_empty() {
                self.proxy = proxy_from_cursor(0, None);
                self.proxy_cursor = 0;
                self.notice = "代理:跟随环境。按 s 验证并完成".into();
                return WizardAction::None;
            }
            if !valid_proxy_url(t) {
                self.input_buf = t.to_string();
                self.typing = true;
                self.typing_proxy = true;
                self.notice =
                    "代理地址要形如 http(s)://host:port 或 socks5(h)://host:port,请重新输入(清空回车 = 跟随环境):".into();
                return WizardAction::None;
            }
            return match znaide_core::config::normalize_proxy_url(t) {
                Ok(n) => {
                    self.proxy = ProxyConfig {
                        mode: ProxyMode::Manual,
                        url: Some(n),
                    };
                    self.proxy_cursor = 2;
                    self.notice = "代理已更新。按 s 验证并完成".into();
                    WizardAction::None
                }
                Err(e) => {
                    self.input_buf = t.to_string();
                    self.typing = true;
                    self.typing_proxy = true;
                    self.notice = format!("{e:#},请重新输入(清空回车 = 跟随环境):");
                    WizardAction::None
                }
            };
        }
        match self.step {
            // 自定义服务商:提交的是端点,先问会话头,确认后才查询模型
            Step::Provider if self.provider == "custom" => {
                if v.is_empty() || !(v.starts_with("http://") || v.starts_with("https://")) {
                    self.notice = "端点要以 http:// 或 https:// 开头,重新输入:".into();
                    self.typing = true;
                    return WizardAction::None;
                }
                self.base_url = v.to_string();
                // custom 无预设开关:默认关(沿用 new() 初值，不从别家带过来)
                self.session_header = false;
                self.enter_session_header();
                WizardAction::None
            }
            // 手动输入模型(查询失败或按 m)
            Step::ModelSelect => {
                if v.is_empty() {
                    self.notice = "模型名不能为空".into();
                    self.typing = true;
                    return WizardAction::None;
                }
                self.model = v.to_string();
                self.enter_protocol_select();
                WizardAction::None
            }
            // 编辑 key(从 ApiKey 进入输入后,仍在 ApiKey 状态,typing 模式)
            _ => {
                self.api_key = v.to_string();
                // 用户手输了明文 → 不再是"来自环境变量",保存会写进配置文件
                self.key_from_env = false;
                self.notice = "key 已更新,按 s 验证连接".into();
                WizardAction::None
            }
        }
    }
}

/// 渲染向导
impl SetupWizard {
    pub fn render(&self, f: &mut ratatui::Frame<'_>, area: Rect) {
        f.render_widget(Clear, area);
        let block = Block::default()
            .title(" 配置向导 — 首次使用需要完成一次配置 ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan));
        let inner = block.inner(area);
        f.render_widget(block, area);

        let mut lines: Vec<Line> = Vec::new();
        // 步骤条
        let steps = [
            "1 服务商",
            "2 会话头",
            "3 模型",
            "4 协议",
            "5 API Key",
            "6 上下文窗口",
            "7 轮数上限",
            "8 弱网重试",
            "9 代理",
            "10 验证",
        ];
        let current = match self.step {
            Step::Provider => 0,
            Step::SessionHeader => 1,
            Step::Querying | Step::ModelSelect => 2,
            Step::ProtocolSelect => 3,
            Step::ApiKey => 4,
            Step::ContextWindow => 5,
            Step::MaxTurns => 6,
            Step::Retry => 7,
            Step::Proxy => 8,
            Step::Verifying => 9,
        };
        let mut bar: Vec<Span> = Vec::new();
        for (i, s) in steps.iter().enumerate() {
            let st = if i == current {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if i < current {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            bar.push(Span::styled(format!("[{s}]"), st));
            if i < steps.len() - 1 {
                bar.push(Span::raw(" → "));
            }
        }
        lines.push(Line::from(bar));
        lines.push(Line::from(""));

        // 重开面板时先把"现在用的是什么"摆出来(首次运行全空则不显示)
        if self.has_configured() {
            let key = if !self.api_key.is_empty() {
                "••••••••".to_string()
            } else if self.key_from_env {
                "来自环境变量".to_string()
            } else {
                "(未设置)".to_string()
            };
            lines.push(Line::from(vec![
                Span::styled("当前配置: ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{} / {}", self.provider, self.model),
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(
                        "  端点 {}  Key {}  窗口 {}  轮数 {}  重试 {}  会话头 {}  代理 {}",
                        self.base_url,
                        key,
                        self.context_window_label(),
                        self.max_turns_label(),
                        self.retry_label(),
                        if self.session_header { "开" } else { "关" },
                        proxy_text_short(&self.proxy),
                    ),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
            lines.push(Line::from(""));
        }

        match self.step {
            Step::Provider => {
                lines.push(Line::from(Span::styled(
                    "选择 AI 服务商(↑↓ 选择,Enter 确认):",
                    Style::default().fg(Color::Yellow),
                )));
                for (i, (name, base, m)) in self.providers.iter().enumerate() {
                    let sel = i == self.cursor;
                    let marker = if sel { "▶ " } else { "  " };
                    let st = if sel {
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    };
                    let default_model = if m.is_empty() {
                        String::new()
                    } else {
                        format!("(默认 {m})")
                    };
                    lines.push(Line::from(vec![
                        Span::styled(marker, Style::default().fg(Color::Cyan)),
                        Span::styled(name.clone(), st),
                        Span::styled(
                            format!("  {base} {default_model}"),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]));
                }
            }
            Step::SessionHeader => {
                lines.push(Line::from(Span::styled(
                    format!(
                        "服务商: {} | 是否启用稳定会话头(x-opencode-session)?",
                        self.provider
                    ),
                    Style::default().fg(Color::Yellow),
                )));
                lines.push(Line::from(Span::styled(
                    "同一会话内所有模型请求携带同一稳定值(会话 ID)，提升服务端 KV 缓存命中率；服务端不识别时直接忽略。",
                    Style::default().fg(Color::DarkGray),
                )));
                for (i, label) in ["是(推荐，缓存命中更高)", "否(默认，不发头)"]
                    .iter()
                    .enumerate()
                {
                    let sel = i == self.session_cursor.min(1);
                    let marker = if sel { "▶ " } else { "  " };
                    let st = if sel {
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    };
                    lines.push(Line::from(vec![
                        Span::styled(marker, Style::default().fg(Color::Cyan)),
                        Span::styled((*label).to_string(), st),
                    ]));
                }
                lines.push(Line::from(Span::styled(
                    "↑↓ 选择 | Y/N 快选 | Enter 下一步(查模型) | Esc 返回改服务商",
                    Style::default().fg(Color::Cyan),
                )));
            }
            Step::Querying => {
                lines.push(Line::from(Span::styled(
                    format!("⏳ {}", self.progress),
                    Style::default().fg(Color::Blue),
                )));
                lines.push(Line::from(Span::styled(
                    "长时间无响应可按 Esc 返回重选",
                    Style::default().fg(Color::DarkGray),
                )));
            }
            Step::ModelSelect => {
                lines.push(Line::from(Span::styled(
                    format!("服务商: {} | 端点: {}", self.provider, self.base_url),
                    Style::default().fg(Color::DarkGray),
                )));
                if !self.models.is_empty() {
                    lines.push(Line::from(Span::styled(
                        "选择模型(↑↓ + Enter),或 m 手动输入:",
                        Style::default().fg(Color::Yellow),
                    )));
                    // 只画一个滚动窗口:光标可能落在 25 行之外,以前固定 take(25) 会让
                    // ▶ 移出屏幕、回车选中看不见的那一项
                    let (first, _) = self.model_window();
                    for (i, m) in self
                        .models
                        .iter()
                        .enumerate()
                        .skip(first)
                        .take(MODEL_WINDOW)
                    {
                        let sel = i == self.model_cursor;
                        let marker = if sel { "▶ " } else { "  " };
                        let st = if sel {
                            Style::default()
                                .fg(Color::White)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(Color::DarkGray)
                        };
                        lines.push(Line::from(vec![
                            Span::styled(marker, Style::default().fg(Color::Cyan)),
                            Span::styled(m.clone(), st),
                        ]));
                    }
                    if self.models.len() > MODEL_WINDOW {
                        lines.push(Line::from(Span::styled(
                            format!(
                                "…(共 {} 个,正在显示 {}-{};↑↓ 继续翻)",
                                self.models.len(),
                                first + 1,
                                (first + MODEL_WINDOW).min(self.models.len())
                            ),
                            Style::default().fg(Color::DarkGray),
                        )));
                    }
                } else {
                    lines.push(Line::from(Span::styled(
                        "该服务商无自动模型列表,直接输入模型名后回车:",
                        Style::default().fg(Color::Yellow),
                    )));
                }
            }
            Step::ProtocolSelect => {
                lines.push(Line::from(Span::styled(
                    format!("服务商: {} | 模型: {}", self.provider, self.model),
                    Style::default().fg(Color::Green),
                )));
                lines.push(Line::from(Span::styled(
                    "协议类型(↑↓ 选择,Enter 确认;默认 chat,直接回车跳过):",
                    Style::default().fg(Color::Yellow),
                )));
                for (i, p) in [ProtocolKind::Chat, ProtocolKind::Response]
                    .iter()
                    .enumerate()
                {
                    let sel = i == self.protocol_cursor.min(1);
                    let marker = if sel { "▶ " } else { "  " };
                    let st = if sel {
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    };
                    lines.push(Line::from(vec![
                        Span::styled(marker, Style::default().fg(Color::Cyan)),
                        Span::styled(p.label(), st),
                    ]));
                }
                lines.push(Line::from(Span::styled(
                    "chat = /chat/completions(默认,兼容最广);response = /responses(部分新模型/端点需要)",
                    Style::default().fg(Color::DarkGray),
                )));
                lines.push(Line::from(Span::styled(
                    "Enter 下一步(API Key)| Esc 返回改模型",
                    Style::default().fg(Color::Cyan),
                )));
            }
            Step::ApiKey => {
                lines.push(Line::from(Span::styled(
                    format!("服务商: {} | 模型: {}", self.provider, self.model),
                    Style::default().fg(Color::Green),
                )));
                let key_disp = if self.key_from_env {
                    "(来自环境变量,保存不会写成明文)".to_string()
                } else if self.api_key.is_empty() {
                    "(未设置)".to_string()
                } else {
                    "••••••••".to_string()
                };
                lines.push(Line::from(vec![
                    Span::styled("API Key: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(key_disp, Style::default().fg(Color::White)),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("轮数上限: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(self.max_turns_label(), Style::default().fg(Color::White)),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("上下文窗口: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        self.context_window_label(),
                        Style::default().fg(Color::White),
                    ),
                ]));
                lines.push(Line::from(Span::styled(
                    "e 输入 key(本地服务可留空)| Enter 下一步(上下文窗口)| s 直接验证并完成 | Esc 返回改协议",
                    Style::default().fg(Color::Cyan),
                )));
            }
            Step::ContextWindow => {
                lines.push(Line::from(Span::styled(
                    format!("服务商: {} | 模型: {}", self.provider, self.model),
                    Style::default().fg(Color::Green),
                )));
                lines.push(Line::from(vec![
                    Span::styled("上下文窗口: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        self.context_window_label(),
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]));
                lines.push(Line::from(Span::styled(
                    "状态栏 ctx 占用条的分母(模型能吃多少 token)。内置表认不出你的模型名时会显示“窗口未知”,只报绝对量",
                    Style::default().fg(Color::DarkGray),
                )));
                lines.push(Line::from(Span::styled(
                    "本地 ollama 请与运行时 num_ctx 一致;直接输入数字修改(留空 = 自动识别)",
                    Style::default().fg(Color::DarkGray),
                )));
                lines.push(Line::from(Span::styled(
                    "Enter 下一步(轮数上限)| s 直接验证并完成 | Esc 返回改 key",
                    Style::default().fg(Color::Cyan),
                )));
            }
            Step::MaxTurns => {
                lines.push(Line::from(Span::styled(
                    format!("服务商: {} | 模型: {}", self.provider, self.model),
                    Style::default().fg(Color::Green),
                )));
                lines.push(Line::from(vec![
                    Span::styled("轮数上限: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        self.max_turns_label(),
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]));
                lines.push(Line::from(Span::styled(
                    "单条消息最多允许的模型往返轮数(一轮可含多次工具调用);用完会问你要不要再放一批",
                    Style::default().fg(Color::DarkGray),
                )));
                lines.push(Line::from(Span::styled(
                    "直接输入数字修改(0 = 不限,留空 = 默认)| Enter 或 s 下一步(弱网重试) | Esc 返回改窗口",
                    Style::default().fg(Color::Cyan),
                )));
            }
            Step::Retry => {
                lines.push(Line::from(Span::styled(
                    format!("服务商: {} | 模型: {}", self.provider, self.model),
                    Style::default().fg(Color::Green),
                )));
                lines.push(Line::from(vec![
                    Span::styled("弱网重试: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        self.retry_label(),
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]));
                lines.push(Line::from(Span::styled(
                    "单次模型调用遇空回复/网络错误时的追加次数(统一退避 800ms*2^n)；耗尽才算彻底失败",
                    Style::default().fg(Color::DarkGray),
                )));
                for (i, label) in [
                    "关闭(默认)",
                    "开启：追加 3 次",
                    "开启：追加 5 次",
                    "开启：追加 8 次",
                    "手动输入次数…",
                ]
                .iter()
                .enumerate()
                {
                    let sel = i == self.retry_cursor.min(4);
                    let marker = if sel { "▶ " } else { "  " };
                    let st = if sel {
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    };
                    lines.push(Line::from(vec![
                        Span::styled(marker, Style::default().fg(Color::Cyan)),
                        Span::styled(label.to_string(), st),
                    ]));
                }
                lines.push(Line::from(Span::styled(
                    "↑↓ 选择档位(手动档回车后输入 0..=8)| Enter 或 s 下一步(网络代理) | Esc 返回改轮数",
                    Style::default().fg(Color::Cyan),
                )));
            }
            Step::Proxy => {
                lines.push(Line::from(Span::styled(
                    format!("服务商: {} | 模型: {}", self.provider, self.model),
                    Style::default().fg(Color::Green),
                )));
                lines.push(Line::from(vec![
                    Span::styled("网络代理: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        proxy_text(&self.proxy),
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]));
                lines.push(Line::from(Span::styled(
                    "全局出口：模型请求 / 网页抓取 / 更新检查统一走这里。本地 ollama 等走 NO_PROXY 豁免，不受影响。",
                    Style::default().fg(Color::DarkGray),
                )));
                for (i, label) in ["跟随环境(默认)", "直连(忽略环境代理)", "手动指定…"]
                    .iter()
                    .enumerate()
                {
                    let sel = i == self.proxy_cursor.min(2);
                    let marker = if sel { "▶ " } else { "  " };
                    let st = if sel {
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    };
                    lines.push(Line::from(vec![
                        Span::styled(marker, Style::default().fg(Color::Cyan)),
                        Span::styled(label.to_string(), st),
                    ]));
                }
                lines.push(Line::from(Span::styled(
                    "↑↓ 选择档位 · y 跟随环境 / n 直连 · 数字/t/e/Enter 手输地址 | s 验证并完成 | Esc 返回改重试",
                    Style::default().fg(Color::Cyan),
                )));
            }
            Step::Verifying => {
                lines.push(Line::from(Span::styled(
                    format!("⏳ {}", self.progress),
                    Style::default().fg(Color::Blue),
                )));
            }
        }

        if self.typing {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled(
                    "输入: ",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    if self.input_buf.is_empty() {
                        "…"
                    } else {
                        self.input_buf.as_str()
                    },
                    Style::default().fg(Color::Green),
                ),
                Span::styled(" _", Style::default().fg(Color::Green)),
            ]));
            lines.push(Line::from(Span::styled(
                "回车确认 | Esc 取消",
                Style::default().fg(Color::DarkGray),
            )));
        } else if !self.notice.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                self.notice.clone(),
                Style::default().fg(Color::Magenta),
            )));
        }

        let p = Paragraph::new(lines)
            .block(Block::default())
            .style(Style::default());
        f.render_widget(p, inner);
    }
}

/// 可用服务商列表:(名称, 端点, 默认模型)
/// need03 复用:quick_cfg 菜单/provider 编辑共用同一份清单。
pub(crate) fn provider_list() -> Vec<(String, String, String)> {
    let cfg = Config::load().unwrap_or_default();
    let mut out: Vec<(String, String, String)> = cfg
        .list_providers()
        .into_iter()
        .map(|(n, base, model)| (n, base, model))
        .collect();
    // 追加"自定义服务商"选项
    out.push(("custom".into(), "(手动输入端点)".into(), String::new()));
    out
}

/// need03 复用(与 SetupWizard::max_turns_label 同文案,供 quick_cfg 调用)。
pub(crate) fn max_turns_text(max_turns: Option<usize>) -> String {
    match max_turns {
        None => format!("默认({} 轮)", znaide_core::session::DEFAULT_MAX_TURNS),
        Some(0) => "不限".to_string(),
        Some(n) => format!("{n} 轮"),
    }
}

/// need03 复用(与 SetupWizard::retry_label 同文案)。
pub(crate) fn retry_text(retry: &RetryConfig) -> String {
    if !retry.enabled {
        "关闭".to_string()
    } else {
        format!("开启(失败后追加 {} 次)", retry.max_retries)
    }
}

/// need03 复用(与 SetupWizard::context_window_label 同文案)。
pub(crate) fn context_window_text(w: Option<usize>) -> String {
    match w {
        None => "自动(按模型名匹配内置表;认不出就只报绝对量)".to_string(),
        Some(n) if n >= 1_000_000 => format!("{n}(约 {}M)", n / 1_000_000),
        Some(n) if n >= 1_000 => format!("{n}(约 {}k)", n / 1_000),
        Some(n) => format!("{n}"),
    }
}

/// need03 复用:重试档位游标→RetryConfig(0=关/1=3/2=5/3=8;4=手动保持 None)。
/// 与 SetupWizard::apply_retry_cursor 同映射。
pub(crate) fn retry_from_cursor(cursor: usize) -> Option<RetryConfig> {
    match cursor {
        0 => Some(RetryConfig::disabled()),
        1 => Some(RetryConfig::enabled_with(3)),
        2 => Some(RetryConfig::enabled_with(5)),
        3 => Some(RetryConfig::enabled_with(8)),
        _ => None,
    }
}

/// need03 复用:数字输入解析(与 SetupWizard::confirm_typed 三路分支同语义)。
/// 上下文窗口:空=None(自动);正整数=Some;0/非数字=Err(回填重输)。
pub(crate) fn parse_context_window_input(t: &str) -> Result<Option<usize>, &'static str> {
    let t = t.trim();
    if t.is_empty() {
        return Ok(None);
    }
    match t.parse::<usize>() {
        Ok(n) if n > 0 => Ok(Some(n)),
        _ => Err("窗口要填正整数 tokens(留空 = 自动识别),请重新输入:"),
    }
}

/// 轮数上限:空=None(默认);0=不限;正整数=Some;非数字=Err。
pub(crate) fn parse_max_turns_input(t: &str) -> Result<Option<usize>, &'static str> {
    let t = t.trim();
    if t.is_empty() {
        return Ok(None);
    }
    match t.parse::<usize>() {
        Ok(n) => Ok(Some(n)),
        Err(_) => Err("轮数上限要填整数(0 = 不限,留空 = 默认),请重新输入:"),
    }
}

/// 弱网重试:空/0=关闭;1..=8 开启(超限夹取 8);非数字=Err。
pub(crate) fn parse_retry_input(t: &str) -> Result<RetryConfig, &'static str> {
    let t = t.trim();
    if t.is_empty() {
        return Ok(RetryConfig::disabled());
    }
    match t.parse::<usize>() {
        Ok(0) => Ok(RetryConfig::disabled()),
        Ok(n) => Ok(RetryConfig::enabled_with(n)),
        Err(_) => Err("重试次数要填 0..=8 的整数(空 = 关闭)，请重新输入:"),
    }
}

/// 端点校验(与 confirm_typed custom 分支同口径)。
pub(crate) fn valid_base_url(v: &str) -> bool {
    let v = v.trim();
    !v.is_empty() && (v.starts_with("http://") || v.starts_with("https://"))
}

/// 代理地址校验（薄包 core，与 valid_base_url 同位置，供 quick_cfg 复用）。
pub(crate) fn valid_proxy_url(v: &str) -> bool {
    znaide_core::config::valid_proxy_url(v)
}

/// 代理档位游标→ProxyConfig（0=跟随环境/1=直连/2=手动保持原 url；与 retry_from_cursor 同画风）。
pub(crate) fn proxy_from_cursor(cursor: usize, url: Option<String>) -> ProxyConfig {
    match cursor {
        1 => ProxyConfig {
            mode: ProxyMode::Off,
            url: None,
        },
        2 => ProxyConfig {
            mode: ProxyMode::Manual,
            url,
        },
        _ => ProxyConfig {
            mode: ProxyMode::Auto,
            url: None,
        },
    }
}

/// 代理三档文案（quick_cfg::current_value 与全向导摘要共用，别各写一遍）。
pub(crate) fn proxy_text(p: &ProxyConfig) -> String {
    match p.mode {
        ProxyMode::Off => "直连(忽略环境代理)".to_string(),
        ProxyMode::Manual => {
            let disp = p
                .url
                .as_deref()
                .map(znaide_core::config::mask_proxy_url)
                .unwrap_or_else(|| "(未填地址)".to_string());
            format!("手动 {disp}")
        }
        ProxyMode::Auto => match znaide_core::update::env_proxy_url() {
            Some(u) => format!("跟随环境(当前 {})", znaide_core::config::mask_proxy_url(&u)),
            None => "跟随环境(当前直连)".to_string(),
        },
    }
}

/// 代理短版（顶部摘要行用，超长截断 24）。
pub(crate) fn proxy_text_short(p: &ProxyConfig) -> String {
    let s = match p.mode {
        ProxyMode::Off => "直连".to_string(),
        ProxyMode::Manual => {
            let disp = p
                .url
                .as_deref()
                .map(znaide_core::config::mask_proxy_url)
                .unwrap_or_else(|| "?".to_string());
            format!("手动:{disp}")
        }
        ProxyMode::Auto => match znaide_core::update::env_proxy_url() {
            Some(u) => format!("环境:{}", znaide_core::config::mask_proxy_url(&u)),
            None => "环境(直连)".to_string(),
        },
    };
    if s.chars().count() > 24 {
        let t: String = s.chars().take(24).collect();
        format!("{t}…")
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn starts_at_provider() {
        let w = SetupWizard::new();
        assert_eq!(w.step, Step::Provider);
        assert!(!w.providers.is_empty());
        // 默认不读盘:轮数上限为空(用默认)
        assert_eq!(w.max_turns, None);
        assert_eq!(
            w.max_turns_label(),
            format!("默认({} 轮)", znaide_core::session::DEFAULT_MAX_TURNS)
        );
    }

    /// 轮数上限是独立一步(第 5 步):数字直接输入、0 = 不限、留空 = 默认、乱填报错重输
    #[test]
    fn max_turns_step_edits_and_verifies() {
        let mut w = SetupWizard::new();
        w.provider = "ollama".into();
        w.model = "qwen3:8b".into();

        // API Key 那步按 Enter → 先到上下文窗口那步,再 Enter → 轮数上限那步
        w.step = Step::ApiKey;
        match w.on_key(key(KeyCode::Enter)) {
            WizardAction::None => {}
            _ => panic!("Enter 应进入下一步"),
        }
        assert_eq!(w.step, Step::ContextWindow);
        w.on_key(key(KeyCode::Enter));
        assert_eq!(w.step, Step::MaxTurns);
        assert_eq!(
            w.max_turns_label(),
            format!("默认({} 轮)", znaide_core::session::DEFAULT_MAX_TURNS)
        );

        // 直接敲数字 5 就进输入(不用先按编辑键),继续敲 00 → 500
        w.on_key(key(KeyCode::Char('5')));
        assert!(w.typing && w.typing_max_turns);
        assert_eq!(w.input_buf, "5");
        for c in "00".chars() {
            w.on_key(key(KeyCode::Char(c)));
        }
        w.on_key(key(KeyCode::Enter));
        assert!(!w.typing && !w.typing_max_turns);
        assert_eq!(w.step, Step::MaxTurns, "提交后留在本步");
        assert_eq!(w.max_turns, Some(500));
        assert_eq!(w.max_turns_label(), "500 轮");

        // 再改成 0 = 不限,t 会预填当前值
        w.on_key(key(KeyCode::Char('t')));
        assert_eq!(w.input_buf, "500");
        w.input_buf.clear();
        w.on_key(key(KeyCode::Char('0')));
        w.on_key(key(KeyCode::Enter));
        assert_eq!(w.max_turns, Some(0));
        assert_eq!(w.max_turns_label(), "不限");

        // 留空 = 回默认
        w.on_key(key(KeyCode::Char('t')));
        w.input_buf.clear();
        w.on_key(key(KeyCode::Enter));
        assert_eq!(w.max_turns, None);

        // 非法输入:留在 typing 并回填原串,不写入值
        w.on_key(key(KeyCode::Char('t')));
        w.input_buf.clear();
        for c in "12x".chars() {
            w.on_key(key(KeyCode::Char(c)));
        }
        w.on_key(key(KeyCode::Enter));
        assert!(w.typing && w.typing_max_turns);
        assert_eq!(w.input_buf, "12x");
        assert_eq!(w.max_turns, None);

        // Esc 退出输入:标记要清掉,免得下次输入被当成轮数
        w.on_key(key(KeyCode::Esc));
        assert!(!w.typing && !w.typing_max_turns);
        assert_eq!(w.step, Step::MaxTurns, "Esc 只退输入,不退步骤");

        // 本步按 Enter → 弱网重试(带上当前轮数上限)
        w.max_turns = Some(300);
        assert!(matches!(w.on_key(key(KeyCode::Enter)), WizardAction::None));
        assert_eq!(w.step, Step::Retry);

        // 重试档位默认关闭；选 5 次后按 s → 网络代理（need03 P4：Retry 之后是 Proxy）
        assert_eq!(w.retry_label(), "关闭");
        w.on_key(key(KeyCode::Down));
        w.on_key(key(KeyCode::Down));
        assert!(w.retry.enabled && w.retry.max_retries == 5);
        assert!(matches!(
            w.on_key(key(KeyCode::Char('s'))),
            WizardAction::None
        ));
        assert_eq!(w.step, Step::Proxy);
        // 代理默认跟随环境；按 s → 验证
        assert_eq!(w.proxy.mode, ProxyMode::Auto);
        match w.on_key(key(KeyCode::Char('s'))) {
            WizardAction::Verify(r) => assert_eq!(r.model, "qwen3:8b"),
            _ => panic!("s 应触发验证"),
        }
        assert_eq!(w.step, Step::Verifying);

        // Esc 从验证回退到代理，再 Esc 回重试，再 Esc 回轮数上限
        w.on_key(key(KeyCode::Esc));
        assert_eq!(w.step, Step::Proxy);
        w.on_key(key(KeyCode::Esc));
        assert_eq!(w.step, Step::Retry);
        w.on_key(key(KeyCode::Esc));
        assert_eq!(w.step, Step::MaxTurns);
    }

    /// 重开面板:已配置的值(服务商/模型/端点/Key/轮数)要带出来,游标落在当前服务商
    #[test]
    fn prefill_shows_existing_config() {
        let mut w = SetupWizard::new();
        assert!(!w.has_configured(), "首次运行没有可展示的配置");
        let cfg = Config {
            provider: Some("deepseek".into()),
            model: Some("deepseek-chat".into()),
            base_url: Some("https://api.deepseek.com/v1".into()),
            api_key: Some("sk-x".into()),
            context_window: None,
            max_turns: Some(500),
            persona: None,
            build_tag: None,
            protocol: None,
            retry: RetryConfig::disabled(),
            proxy: ProxyConfig::default(),
            providers: Default::default(),
        };
        w.apply_config(&cfg);
        assert_eq!(w.provider, "deepseek");
        assert_eq!(w.model, "deepseek-chat");
        assert_eq!(w.base_url, "https://api.deepseek.com/v1");
        assert_eq!(w.api_key, "sk-x");
        assert_eq!(w.max_turns, Some(500));
        assert!(!w.key_from_env);
        assert!(w.has_configured());
        // 游标落在当前服务商上(列表里就能看出现在用的是哪个)
        if let Some(i) = w.providers.iter().position(|(n, _, _)| n == "deepseek") {
            assert_eq!(w.cursor, i);
        }
        // 直接 Enter 选同一个服务商:已配好的 key 不能被清掉
        w.on_key(key(KeyCode::Enter));
        assert_eq!(w.api_key, "sk-x", "重选同一个服务商不该清掉已配的 key");
    }

    /// 用 api_key_env 的配置:key 不预填成明文(否则保存会写回文件),但要标出来源
    #[test]
    fn prefill_keeps_env_key_out_of_panel() {
        const VAR: &str = "ZNAIDE_TEST_CFG_KEY";
        std::env::set_var(VAR, "sk-from-env");
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "testprov".into(),
            znaide_core::config::ProviderDef {
                base_url: Some("https://t/v1".into()),
                model: Some("m1".into()),
                api_key_env: Some(VAR.into()),
                api_key: None,
                context_window: None,
                protocol: None,
                session_header: false,
            },
        );
        let cfg = Config {
            provider: Some("testprov".into()),
            model: Some("m1".into()),
            base_url: Some("https://t/v1".into()),
            api_key: None,
            context_window: None,
            max_turns: None,
            persona: None,
            build_tag: None,
            protocol: None,
            retry: RetryConfig::disabled(),
            proxy: ProxyConfig::default(),
            providers,
        };
        let mut w = SetupWizard::new();
        w.apply_config(&cfg);
        assert!(w.api_key.is_empty(), "环境变量里的 key 不该被预填成明文");
        assert!(w.key_from_env, "要标出 key 来自环境变量");
        assert_eq!(w.model, "m1");
        assert_eq!(w.max_turns, None);
        std::env::remove_var(VAR);
    }

    /// 粘贴只填充输入缓冲、不触发任何动作:多行/含尾换行压成单行,回车才算提交
    #[test]
    fn paste_fills_typing_buffer_without_triggering() {
        // 端点(custom 流程):粘贴带尾换行的 URL,不该把换行带进去
        let mut w = SetupWizard::new();
        w.provider = "custom".into();
        w.step = Step::Provider;
        w.typing = true;
        w.input_buf.clear();
        assert!(w.paste_text("https://api.example.com/v1\r\n"));
        assert_eq!(w.input_buf, "https://api.example.com/v1");
        // 再粘一段是追加;多行粘贴的换行被丢掉(Key 这种长串最怕带换行)
        assert!(w.paste_text("sk-abc\ndef\r\ngh"));
        assert_eq!(w.input_buf, "https://api.example.com/v1sk-abcdefgh");
        assert!(w.typing, "粘贴不该结束输入态");
        assert_eq!(w.step, Step::Provider, "粘贴不该推进步骤");
        // 仍要按回车才提交(先到会话头选择，再确认才查模型)
        match w.on_key(key(KeyCode::Enter)) {
            WizardAction::None => {}
            _ => panic!("回车先到会话头选择"),
        }
        assert_eq!(w.step, Step::SessionHeader);
        match w.on_key(key(KeyCode::Enter)) {
            WizardAction::FetchModels { base_url, .. } => {
                assert_eq!(base_url, "https://api.example.com/v1sk-abcdefgh")
            }
            _ => panic!("确认后才触发查询"),
        }

        // API Key 输入态同样能粘
        let mut k = SetupWizard::new();
        k.step = Step::ApiKey;
        k.typing = true;
        k.input_buf.clear();
        assert!(k.paste_text("sk-proj-1234567890\n"));
        assert_eq!(k.input_buf, "sk-proj-1234567890");

        // 非输入态(选择列表 / 等待验证)不接受粘贴
        let mut l = SetupWizard::new();
        assert!(!l.paste_text("x"));
        assert!(l.input_buf.is_empty());
    }

    /// 回归:配好 A、再配 B 之后切回 A,面板必须带出 **A 自己**的模型与 Key
    /// (曾经会沿用 B 的模型/B 的 Key,甚至在保存时把 B 的模型写进 A 的条目)
    #[test]
    fn switching_back_loads_that_provider_settings() {
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "deepseek".into(),
            znaide_core::config::ProviderDef {
                base_url: Some("https://api.deepseek.com/v1".into()),
                model: Some("deepseek-flash".into()),
                api_key: Some("sk-deepseek".into()),
                context_window: Some(1_048_576),
                ..Default::default()
            },
        );
        providers.insert(
            "dashscope".into(),
            znaide_core::config::ProviderDef {
                base_url: Some("https://dashscope.aliyuncs.com/compatible-mode/v1".into()),
                model: Some("qwen-plus".into()),
                api_key: Some("sk-aliyun".into()),
                context_window: Some(40_960),
                ..Default::default()
            },
        );
        // 当前生效的是阿里云(用户刚配完)
        let cfg = Config {
            provider: Some("dashscope".into()),
            ..Default::default()
        };
        // apply_config 只认 cfg.providers 里的表,这里直接把表塞进 Config
        let cfg = Config { providers, ..cfg };

        let mut w = SetupWizard::new();
        w.apply_config(&cfg);
        assert_eq!(w.provider, "dashscope");
        assert_eq!(w.model, "qwen-plus");
        assert_eq!(w.api_key, "sk-aliyun");
        assert_eq!(w.context_window, Some(40_960), "窗口也是这家自己的");

        // 切回 deepseek:模型与 Key 都该换成 deepseek 自己的
        let ds = w
            .providers
            .iter()
            .position(|(n, _, _)| n == "deepseek")
            .expect("列表里应有 deepseek");
        w.cursor = ds;
        w.on_key(key(KeyCode::Enter));
        assert_eq!(w.provider, "deepseek");
        assert_eq!(w.model, "deepseek-flash", "不能还留着阿里云的模型");
        assert_eq!(w.api_key, "sk-deepseek", "不能还留着阿里云的 Key");
        assert_eq!(
            w.base_url, "https://api.deepseek.com/v1",
            "端点要跟着服务商走"
        );
        assert_eq!(
            w.context_window,
            Some(1_048_576),
            "窗口不能沿用上一家的 40k"
        );
        assert!(!w.key_from_env, "明文 Key 不该被标成来自环境变量");
        assert!(w.models.is_empty(), "换家后旧模型列表要清掉");

        // 再切回阿里云,同样各归各家(切之前先把步骤拨回选择服务商)
        w.step = Step::Provider;
        let al = w
            .providers
            .iter()
            .position(|(n, _, _)| n == "dashscope")
            .unwrap();
        w.cursor = al;
        w.on_key(key(KeyCode::Enter));
        assert_eq!(w.model, "qwen-plus");
        assert_eq!(w.api_key, "sk-aliyun");
        assert_eq!(w.context_window, Some(40_960));
    }

    /// 服务商给出的模型列表里没有"当前模型"时,保留它并置顶选中(避免回车被悄悄换掉)
    #[test]
    fn inject_models_keeps_current_model_on_top() {
        let mut w = SetupWizard::new();
        w.provider = "deepseek".into();
        w.model = "deepseek-flash".into();
        w.step = Step::Querying;
        w.inject_models(Ok(vec!["deepseek-chat".into(), "deepseek-reasoner".into()]));
        assert_eq!(w.models.first().map(|s| s.as_str()), Some("deepseek-flash"));
        assert_eq!(w.model_cursor, 0, "当前模型应被选中");
        // 回车确认后仍是当前模型,没被列表第一项覆盖
        w.on_key(key(KeyCode::Enter));
        assert_eq!(w.model, "deepseek-flash");
    }

    /// 条目没有明文 Key 但有 api_key_env:切过去时从环境变量取
    #[test]
    fn switching_provider_takes_key_from_env_when_no_plaintext() {
        const VAR: &str = "ZNAIDE_TEST_SWITCH_KEY";
        std::env::set_var(VAR, "sk-env-switch");
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "dashscope".into(),
            znaide_core::config::ProviderDef {
                base_url: Some("https://dashscope.aliyuncs.com/compatible-mode/v1".into()),
                model: Some("qwen-plus".into()),
                api_key_env: Some(VAR.into()),
                ..Default::default()
            },
        );
        let cfg = Config {
            provider: Some("ollama".into()),
            providers,
            ..Default::default()
        };
        let mut w = SetupWizard::new();
        w.apply_config(&cfg);
        let al = w
            .providers
            .iter()
            .position(|(n, _, _)| n == "dashscope")
            .unwrap();
        w.cursor = al;
        w.on_key(key(KeyCode::Enter));
        assert_eq!(w.model, "qwen-plus");
        assert_eq!(w.api_key, "sk-env-switch");
        assert!(w.key_from_env, "应标注为来自环境变量");
        std::env::remove_var(VAR);
    }

    #[test]
    fn select_provider_fetches_models() {
        let mut w = SetupWizard::new();
        // 找到 deepseek(内置),Enter → 先到会话头选择(默认否)
        if let Some(i) = w.providers.iter().position(|(n, _, _)| n == "deepseek") {
            w.cursor = i;
        }
        match w.on_key(key(KeyCode::Enter)) {
            WizardAction::None => {}
            _ => panic!("选完供应商应先到会话头选择，不直接查模型"),
        }
        assert_eq!(w.step, Step::SessionHeader);
        assert_eq!(w.provider, "deepseek");
        assert!(!w.session_header);
        // 默认否 → Enter 确认后才查模型，且不带探针头
        match w.on_key(key(KeyCode::Enter)) {
            WizardAction::FetchModels {
                base_url,
                session_header,
                ..
            } => {
                assert!(base_url.contains("deepseek"));
                assert!(session_header.is_none(), "开关关 → 查询不带头");
            }
            _ => panic!("应触发模型查询"),
        }
        assert_eq!(w.step, Step::Querying);
        assert_eq!(w.provider, "deepseek");
    }

    /// 会话头步骤:Provider Enter → SessionHeader;Y 开/N 关/↑↓切;Enter 确认后查模型;
    /// 开时 FetchModels 带 wizard- 探针头;draft 带出开关;Esc 逐级回退
    #[test]
    fn session_header_step_flow() {
        let mut w = SetupWizard::new();
        if let Some(i) = w.providers.iter().position(|(n, _, _)| n == "deepseek") {
            w.cursor = i;
        }
        w.on_key(key(KeyCode::Enter));
        assert_eq!(w.step, Step::SessionHeader);
        // j 切到否(默认已是否)，k 切回是
        w.on_key(key(KeyCode::Char('k')));
        assert!(w.session_header);
        assert_eq!(w.session_cursor, 0);
        w.on_key(key(KeyCode::Char('j')));
        assert!(!w.session_header);
        // Y 直达是 → 直接查模型且带探针头
        match w.on_key(key(KeyCode::Char('y'))) {
            WizardAction::FetchModels { session_header, .. } => {
                let v = session_header.expect("开 → 查询带探针头");
                assert!(v.starts_with("wizard-"), "got: {v}");
            }
            _ => panic!("Y 应确认并查模型"),
        }
        assert!(w.session_header);
        assert_eq!(w.step, Step::Querying);
        // Esc 回到会话头选择
        w.on_key(key(KeyCode::Esc));
        assert_eq!(w.step, Step::SessionHeader);
        // N 直达否 → 查模型不带头
        match w.on_key(key(KeyCode::Char('N'))) {
            WizardAction::FetchModels { session_header, .. } => {
                assert!(session_header.is_none());
            }
            _ => panic!("N 应确认并查模型"),
        }
        assert!(!w.session_header);
        // draft 带出开关
        assert!(!w.draft().session_header_enabled);
    }

    /// 换家带开关:A 家开 → 切 B 家(关)不串家;draft 同步
    #[test]
    fn switching_provider_carries_session_header() {
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "a".into(),
            znaide_core::config::ProviderDef {
                model: Some("ma".into()),
                session_header: true,
                ..Default::default()
            },
        );
        providers.insert(
            "b".into(),
            znaide_core::config::ProviderDef {
                model: Some("mb".into()),
                ..Default::default()
            },
        );
        let mut w = SetupWizard::new();
        w.provider_defs = providers;
        w.apply_provider_defaults("a");
        assert!(w.session_header);
        assert_eq!(w.session_cursor, 0);
        assert!(w.draft().session_header_enabled);
        w.apply_provider_defaults("b");
        assert!(!w.session_header);
        assert_eq!(w.session_cursor, 1);
        assert!(!w.draft().session_header_enabled);
    }

    /// apply_config 从条目带出开关
    #[test]
    fn apply_config_carries_session_header() {
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "a".into(),
            znaide_core::config::ProviderDef {
                base_url: Some("https://a/v1".into()),
                model: Some("ma".into()),
                session_header: true,
                ..Default::default()
            },
        );
        let cfg = Config {
            provider: Some("a".into()),
            providers,
            ..Default::default()
        };
        let mut w = SetupWizard::new();
        w.apply_config(&cfg);
        assert!(w.session_header);
        assert!(w.draft().session_header_enabled);
    }

    #[test]
    fn model_injection_then_protocol_step() {
        let mut w = SetupWizard::new();
        w.provider = "ollama".into();
        w.step = Step::Querying;
        w.inject_models(Ok(vec!["qwen3:8b".into(), "llama3".into()]));
        assert_eq!(w.step, Step::ModelSelect);
        assert_eq!(w.models.len(), 2);
        // 选第二个模型
        w.model_cursor = 1;
        match w.on_key(key(KeyCode::Enter)) {
            WizardAction::None => {}
            _ => panic!(),
        }
        assert_eq!(w.model, "llama3");
        // 选完模型先到协议选择(默认 chat),不再直达 ApiKey
        assert_eq!(w.step, Step::ProtocolSelect);
        assert_eq!(w.protocol, ProtocolKind::Chat);
        assert_eq!(w.protocol_cursor, 0);
    }

    /// 协议步骤全流程:ModelSelect Enter → ProtocolSelect;j 切 response;
    /// Enter → ApiKey;Esc 逐级回退;draft 带出协议;apply_config 预填落在已存值
    #[test]
    fn protocol_step_flow() {
        let mut w = SetupWizard::new();
        w.provider = "ollama".into();
        w.step = Step::Querying;
        w.inject_models(Ok(vec!["qwen3:8b".into()]));
        w.on_key(key(KeyCode::Enter));
        assert_eq!(w.step, Step::ProtocolSelect);

        // j 切到 response(所见即所得,移动即赋值)
        w.on_key(key(KeyCode::Char('j')));
        assert_eq!(w.protocol, ProtocolKind::Response);
        assert_eq!(w.protocol_cursor, 1);
        // k 切回 chat
        w.on_key(key(KeyCode::Char('k')));
        assert_eq!(w.protocol, ProtocolKind::Chat);
        w.on_key(key(KeyCode::Char('j')));
        assert_eq!(w.protocol, ProtocolKind::Response);

        // Enter → ApiKey;draft 带出协议
        w.on_key(key(KeyCode::Enter));
        assert_eq!(w.step, Step::ApiKey);
        assert_eq!(w.draft().protocol, ProtocolKind::Response);

        // Esc 回 ProtocolSelect(保留 response),再 Esc 回 ModelSelect
        w.on_key(key(KeyCode::Esc));
        assert_eq!(w.step, Step::ProtocolSelect);
        assert_eq!(w.protocol, ProtocolKind::Response);
        w.on_key(key(KeyCode::Esc));
        assert_eq!(w.step, Step::ModelSelect);

        // apply_config 预填落在已存值上
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "ollama".into(),
            znaide_core::config::ProviderDef {
                base_url: Some("http://127.0.0.1:11434/v1".into()),
                model: Some("qwen3:8b".into()),
                protocol: Some(ProtocolKind::Response),
                ..Default::default()
            },
        );
        let cfg = Config {
            provider: Some("ollama".into()),
            providers,
            ..Default::default()
        };
        let mut w2 = SetupWizard::new();
        w2.apply_config(&cfg);
        assert_eq!(w2.protocol, ProtocolKind::Response);
        assert_eq!(w2.protocol_cursor, 1);
        assert_eq!(w2.draft().protocol, ProtocolKind::Response);
    }

    #[test]
    fn query_failure_allows_manual_model() {
        let mut w = SetupWizard::new();
        w.provider = "custom".into();
        w.step = Step::Querying;
        w.inject_models(Err("网络错误".into()));
        assert_eq!(w.step, Step::ModelSelect);
        assert!(w.typing); // 进入手输
                           // 直接输入模型名
        for c in "my-model".chars() {
            w.on_key(key(KeyCode::Char(c)));
        }
        w.on_key(key(KeyCode::Enter));
        assert_eq!(w.model, "my-model");
        assert_eq!(w.step, Step::ProtocolSelect);
    }

    #[test]
    fn custom_provider_enters_endpoint() {
        let mut w = SetupWizard::new();
        // custom 是最后一个
        w.cursor = w.providers.len() - 1;
        w.on_key(key(KeyCode::Enter));
        assert!(w.typing); // 等待输入端点
                           // 非法端点被拒绝
        for c in "not-a-url".chars() {
            w.on_key(key(KeyCode::Char(c)));
        }
        w.on_key(key(KeyCode::Enter));
        assert!(w.typing); // 仍在输入(校验失败)
        assert!(w.notice.contains("http"));
        // Backspace 清空(not-a-url 共 9 字符)
        for _ in 0..9 {
            w.on_key(key(KeyCode::Backspace));
        }
        for c in "http://127.0.0.1:11434/v1".chars() {
            w.on_key(key(KeyCode::Char(c)));
        }
        // 端点提交后先到会话头选择(不直接查模型)
        match w.on_key(key(KeyCode::Enter)) {
            WizardAction::None => {}
            _ => panic!("custom 端点提交后应先到会话头选择"),
        }
        assert_eq!(w.step, Step::SessionHeader);
        assert_eq!(w.base_url, "http://127.0.0.1:11434/v1");
        // 确认(默认否)后才查模型
        match w.on_key(key(KeyCode::Enter)) {
            WizardAction::FetchModels { base_url, .. } => {
                assert_eq!(base_url, "http://127.0.0.1:11434/v1");
            }
            _ => panic!("应触发模型查询"),
        }
    }

    #[test]
    fn api_key_edit_and_verify() {
        let mut w = SetupWizard::new();
        w.provider = "deepseek".into();
        w.model = "deepseek-chat".into();
        w.step = Step::ApiKey;
        // e 编辑 key
        w.on_key(key(KeyCode::Char('e')));
        assert!(w.typing);
        for c in "sk-test-123".chars() {
            w.on_key(key(KeyCode::Char(c)));
        }
        w.on_key(key(KeyCode::Enter));
        assert_eq!(w.api_key, "sk-test-123");
        // s 验证
        match w.on_key(key(KeyCode::Char('s'))) {
            WizardAction::Verify(r) => {
                assert_eq!(r.model, "deepseek-chat");
                assert_eq!(r.api_key.as_deref(), Some("sk-test-123"));
            }
            _ => panic!("应触发验证"),
        }
        assert_eq!(w.step, Step::Verifying);
    }

    #[test]
    fn verify_result_updates_notice() {
        let mut w = SetupWizard::new();
        w.step = Step::Verifying;
        w.inject_verify(false, "401 Unauthorized".into());
        assert_eq!(w.step, Step::ApiKey);
        assert!(w.notice.contains("401"));
    }

    /// 上下文窗口是独立一步(第 4 步):填正整数 = 固定值,留空 = 自动识别(内置表/未知)
    #[test]
    fn context_window_step_edits_and_verifies() {
        let mut w = SetupWizard::new();
        w.provider = "deepseek".into();
        w.model = "deepseek-flash".into();
        w.step = Step::ContextWindow;
        assert_eq!(w.context_window, None);
        assert!(w.context_window_label().contains("自动"));

        // 直接敲数字就进输入(不用先按编辑键),继续敲完 → 1048576
        w.on_key(key(KeyCode::Char('1')));
        assert!(w.typing && w.typing_context_window);
        assert_eq!(w.input_buf, "1");
        for c in "048576".chars() {
            w.on_key(key(KeyCode::Char(c)));
        }
        w.on_key(key(KeyCode::Enter));
        assert!(!w.typing && !w.typing_context_window);
        assert_eq!(w.step, Step::ContextWindow, "提交后留在本步");
        assert_eq!(w.context_window, Some(1_048_576));
        assert!(w.context_window_label().contains('M'));

        // t 预填当前值,改成小窗口(40k 的本地模型典型值)
        w.on_key(key(KeyCode::Char('t')));
        assert_eq!(w.input_buf, "1048576");
        w.input_buf.clear();
        for c in "40960".chars() {
            w.on_key(key(KeyCode::Char(c)));
        }
        w.on_key(key(KeyCode::Enter));
        assert_eq!(w.context_window, Some(40_960));
        assert!(w.context_window_label().contains("40k"));

        // 留空 = 回到"自动"(按模型名查内置表)
        w.on_key(key(KeyCode::Char('t')));
        w.input_buf.clear();
        w.on_key(key(KeyCode::Enter));
        assert_eq!(w.context_window, None);

        // 非法输入(0 / 非数字):不写入,留输入态并回填原串
        for bad in ["0", "abc"] {
            w.on_key(key(KeyCode::Char('t')));
            w.input_buf.clear();
            for c in bad.chars() {
                w.on_key(key(KeyCode::Char(c)));
            }
            w.on_key(key(KeyCode::Enter));
            assert!(w.typing && w.typing_context_window, "{bad} 应留在输入态");
            assert_eq!(w.input_buf, bad);
            assert_eq!(w.context_window, None, "{bad} 不是有效窗口,不该写入");
            w.on_key(key(KeyCode::Esc));
            assert!(
                !w.typing && !w.typing_context_window,
                "Esc 要清掉窗口输入标记"
            );
        }

        // 本步 Enter → 轮数上限;draft 带上窗口(保存路径要用它)
        w.context_window = Some(131_072);
        w.on_key(key(KeyCode::Enter));
        assert_eq!(w.step, Step::MaxTurns);
        assert_eq!(w.draft().context_window, Some(131_072));

        // Esc 链:验证 → 网络代理 → 弱网重试 → 轮数上限 → 上下文窗口
        w.step = Step::Verifying;
        w.on_key(key(KeyCode::Esc));
        assert_eq!(w.step, Step::Proxy);
        w.on_key(key(KeyCode::Esc));
        assert_eq!(w.step, Step::Retry);
        w.on_key(key(KeyCode::Esc));
        assert_eq!(w.step, Step::MaxTurns);
        w.on_key(key(KeyCode::Esc));
        assert_eq!(w.step, Step::ContextWindow);
    }

    /// need03 U11:全向导代理步骤链（Retry -Enter→ Proxy -s→ Verifying -Esc→ Proxy -Esc→ Retry；
    /// 档位键/手输/回退与 quick_cfg 同口径）
    #[test]
    fn proxy_step_chain_edits_and_verifies() {
        let mut w = SetupWizard::new();
        w.provider = "ollama".into();
        w.model = "qwen3:8b".into();
        w.step = Step::Retry;
        // Enter → 代理步
        assert!(matches!(w.on_key(key(KeyCode::Enter)), WizardAction::None));
        assert_eq!(w.step, Step::Proxy);
        // ↓ 到直连即时落值
        w.on_key(key(KeyCode::Down));
        assert_eq!(w.proxy.mode, ProxyMode::Off);
        assert_eq!(w.draft().proxy, znaide_core::config::EffectiveProxy::Direct);
        // y 回跟随环境
        w.on_key(key(KeyCode::Char('y')));
        assert_eq!(w.proxy.mode, ProxyMode::Auto);
        // 手输合法地址进 Manual（draft 落定带上待存代理）
        w.on_key(key(KeyCode::Char('e')));
        assert!(w.typing && w.typing_proxy);
        for c in "http://127.0.0.1:10808".chars() {
            w.on_key(key(KeyCode::Char(c)));
        }
        w.on_key(key(KeyCode::Enter));
        assert_eq!(w.proxy.mode, ProxyMode::Manual);
        assert_eq!(
            w.draft().proxy,
            znaide_core::config::EffectiveProxy::Via("http://127.0.0.1:10808".into())
        );
        // s → 验证（探针走待存代理）
        match w.on_key(key(KeyCode::Char('s'))) {
            WizardAction::Verify(r) => assert_eq!(
                r.proxy,
                znaide_core::config::EffectiveProxy::Via("http://127.0.0.1:10808".into())
            ),
            _ => panic!("s 应触发验证"),
        }
        assert_eq!(w.step, Step::Verifying);
        // Esc 回代理，再 Esc 回重试
        w.on_key(key(KeyCode::Esc));
        assert_eq!(w.step, Step::Proxy);
        w.on_key(key(KeyCode::Esc));
        assert_eq!(w.step, Step::Retry);
    }
}
