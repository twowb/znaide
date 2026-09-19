//! `/cfg` 按需直改面板:菜单选一项、独立编辑、`s` 直接提交 / `v` 验证后提交。
//! 与 `config_ui::SetupWizard` 线性向导并存:复用它的输入解析/夹取/标签文案
//! (`config_ui::parse_*` / `retry_from_cursor` / `*_text` / `provider_list`),
//! 状态机自建(`QcStep`),不扩展 `Step`(见 docs/need03 05)。
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use znaide_core::config::{
    Config, EffectiveProxy, ProtocolKind, ProviderDef, ProxyConfig, ProxyMode, Resolved,
    RetryConfig,
};

use crate::config_ui::{
    context_window_text, max_turns_text, parse_context_window_input, parse_max_turns_input,
    parse_retry_input, provider_list, proxy_from_cursor, proxy_text, proxy_text_short,
    retry_from_cursor, retry_text, valid_base_url, valid_proxy_url,
};

/// 按需改 10 项(02 §1 矩阵:C1–C9 + need03 代理 C10)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QcItem {
    Provider,
    Model,
    Protocol,
    ApiKey,
    BaseUrl,
    ContextWindow,
    MaxTurns,
    Retry,
    SessionHeader,
    Proxy,
}

impl QcItem {
    pub const ALL: [QcItem; 10] = [
        QcItem::Provider,
        QcItem::Model,
        QcItem::Protocol,
        QcItem::ApiKey,
        QcItem::BaseUrl,
        QcItem::ContextWindow,
        QcItem::MaxTurns,
        QcItem::Retry,
        QcItem::SessionHeader,
        QcItem::Proxy,
    ];

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|i| *i == self).unwrap_or(0)
    }

    pub fn from_index(i: usize) -> QcItem {
        Self::ALL[i.min(Self::ALL.len() - 1)]
    }

    pub fn title(self) -> &'static str {
        match self {
            QcItem::Provider => "服务商切换",
            QcItem::Model => "模型",
            QcItem::Protocol => "协议类型",
            QcItem::ApiKey => "API Key",
            QcItem::BaseUrl => "端点 base_url",
            QcItem::ContextWindow => "上下文窗口",
            QcItem::MaxTurns => "轮次上限",
            QcItem::Retry => "弱网重试",
            QcItem::SessionHeader => "会话头开关",
            QcItem::Proxy => "网络代理",
        }
    }

    /// 本地项(默认直接提交) vs 连接项(默认验证后提交),见 02 §2。
    pub fn is_local(self) -> bool {
        matches!(
            self,
            QcItem::ContextWindow
                | QcItem::MaxTurns
                | QcItem::Retry
                | QcItem::SessionHeader
        )
    }

    pub fn default_submit(self) -> SubmitKind {
        if self.is_local() {
            SubmitKind::Direct
        } else {
            SubmitKind::Verified
        }
    }

    pub fn tag(self) -> &'static str {
        if self.is_local() {
            "[本地]"
        } else {
            "[建议验证]"
        }
    }
}

/// `/config <子项>` / `/cfg <子项>` 别名(02 §5):大小写不敏感,`-`/`_` 等价,含中文。
/// 未命中 → None(宿主落菜单 + Notice,不报错退出)。
pub fn parse_qc_alias(s: &str) -> Option<QcItem> {
    let norm = s.trim().to_lowercase().replace('_', "-");
    match norm.as_str() {
        "retry" | "重试" => Some(QcItem::Retry),
        "window" | "ctx" | "上下文" | "context" | "context-window" => Some(QcItem::ContextWindow),
        "turns" | "max-turns" | "轮次" | "maxturns" => Some(QcItem::MaxTurns),
        "key" | "apikey" | "api-key" => Some(QcItem::ApiKey),
        "model" | "模型" => Some(QcItem::Model),
        "url" | "base-url" | "baseurl" | "端点" => Some(QcItem::BaseUrl),
        "protocol" | "协议" => Some(QcItem::Protocol),
        "provider" | "服务商" => Some(QcItem::Provider),
        "session-header" | "sessionheader" | "会话头" => Some(QcItem::SessionHeader),
        "proxy" | "代理" | "网络代理" | "network-proxy" => Some(QcItem::Proxy),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QcStep {
    Menu,
    Edit(QcItem),
    Verifying { item: QcItem },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitKind {
    Direct,
    Verified,
}

/// 宿主需执行的动作(与 WizardAction 对称,app.rs 执行保存/验证/查模型)。
pub enum QuickCfgAction {
    None,
    Exit,
    Save(QcItem),
    Verify(Resolved),
    FetchModels {
        base_url: String,
        api_key: Option<String>,
        session_header: Option<String>,
        proxy: EffectiveProxy,
    },
}

pub struct QuickCfgPanel {
    pub step: QcStep,
    pub menu_cursor: usize,
    /// provider 列表(名称,端点,默认模型,含 custom 尾项;与全向导同源)
    pub providers: Vec<(String, String, String)>,
    pub cursor: usize,
    pub provider: String,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub models: Vec<String>,
    pub model_cursor: usize,
    pub protocol: ProtocolKind,
    pub protocol_cursor: usize,
    pub session_header: bool,
    pub session_cursor: usize,
    pub context_window: Option<usize>,
    pub max_turns: Option<usize>,
    pub retry: RetryConfig,
    pub retry_cursor: usize,
    /// 全局网络代理三档原文 + 档位游标（0=跟随环境/1=直连/2=手动指定）
    pub proxy: ProxyConfig,
    pub proxy_cursor: usize,
    pub typing: bool,
    pub input_buf: String,
    /// custom 家正在输入端点(与 provider 列表的 custom 选项配合)
    pub typing_custom_url: bool,
    pub typing_context_window: bool,
    pub typing_max_turns: bool,
    pub typing_retry: bool,
    /// 代理手动档正在输入 URL（与 typing_retry 同路，共用 typing/input_buf 通道）
    pub typing_proxy: bool,
    pub key_from_env: bool,
    pub key_env_name: Option<String>,
    pub provider_defs: std::collections::HashMap<String, ProviderDef>,
    pub notice: String,
    pub progress: String,
}

impl QuickCfgPanel {
    /// 空参入口:菜单首屏。先 prefill(读盘),测试用 apply_config 注入。
    pub fn open_menu() -> Self {
        let mut p = Self::new();
        p.prefill();
        p
    }

    /// 直达入口:跳过菜单进指定编辑器(模型项自动触发一次列表查询)。
    pub fn open_with(item: QcItem) -> Self {
        let mut p = Self::open_menu();
        p.enter_edit(item);
        p
    }

    fn new() -> Self {
        Self {
            step: QcStep::Menu,
            menu_cursor: 0,
            providers: provider_list(),
            cursor: 0,
            provider: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            models: Vec::new(),
            model_cursor: 0,
            protocol: ProtocolKind::default(),
            protocol_cursor: 0,
            session_header: false,
            session_cursor: 1,
            context_window: None,
            max_turns: None,
            retry: RetryConfig::disabled(),
            retry_cursor: 0,
            proxy: ProxyConfig::default(),
            proxy_cursor: 0,
            typing: false,
            input_buf: String::new(),
            typing_custom_url: false,
            typing_context_window: false,
            typing_max_turns: false,
            typing_retry: false,
            typing_proxy: false,
            key_from_env: false,
            key_env_name: None,
            provider_defs: std::collections::HashMap::new(),
            notice: String::new(),
            progress: String::new(),
        }
    }

    /// 用现有配置预填(不在 new 里读盘的逻辑部分,便于测试)。
    /// 口径照抄 SetupWizard::apply_config:服务商游标/模型/端点/Key 三态/
    /// 轮数/窗口/协议/会话头/重试档位全带出。
    pub fn apply_config(&mut self, cfg: &Config) {
        let r = cfg.resolve(None, None, None, None, None, None).ok();
        let provider = cfg
            .provider
            .clone()
            .filter(|p| !p.is_empty())
            .or_else(|| r.as_ref().map(|x| x.provider_name.clone()));
        if let Some(p) = provider {
            self.provider = p.clone();
            if let Some(i) = self.providers.iter().position(|(n, _, _)| *n == p) {
                self.cursor = i;
            }
        }
        if let Some(x) = &r {
            self.model = x.model.clone();
            self.base_url = x.base_url.clone();
        }
        let def = cfg.all_providers().get(&self.provider).cloned();
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
        self.key_from_env = top_plain.is_none()
            && entry_plain.is_none()
            && def
                .as_ref()
                .map(|d| d.api_key_env.is_some())
                .unwrap_or(false);
        self.key_env_name = if self.key_from_env {
            def.as_ref().and_then(|d| d.api_key_env.clone())
        } else {
            None
        };
        self.max_turns = cfg.max_turns;
        self.context_window = def.as_ref().and_then(|d| d.context_window);
        self.protocol = cfg
            .protocol
            .or_else(|| def.as_ref().and_then(|d| d.protocol))
            .unwrap_or_default();
        self.protocol_cursor = match self.protocol {
            ProtocolKind::Chat => 0,
            ProtocolKind::Response => 1,
        };
        self.session_header = def.as_ref().map(|d| d.session_header).unwrap_or(false);
        self.session_cursor = if self.session_header { 0 } else { 1 };
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
        // 代理全局（切家不重置，apply_provider_defaults 不碰它）
        let mut proxy = cfg.proxy.clone();
        // 归一：mode 非 manual → url=None 显示干净
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

    pub fn prefill(&mut self) {
        if let Ok(cfg) = Config::load() {
            self.apply_config(&cfg);
        }
    }

    /// 是否已有配置(首次运行全空 → 宿主拒绝进入,见 03 §1)。
    pub fn has_configured(&self) -> bool {
        !self.provider.is_empty() || !self.model.is_empty() || !self.api_key.is_empty()
    }

    fn enter_edit(&mut self, item: QcItem) {
        self.step = QcStep::Edit(item);
        self.typing = false;
        self.input_buf.clear();
        self.typing_custom_url = false;
        self.typing_context_window = false;
        self.typing_max_turns = false;
        self.typing_retry = false;
        self.typing_proxy = false;
        self.notice.clear();
        match item {
            QcItem::Model => {
                if let Some(i) = self.models.iter().position(|m| *m == self.model) {
                    self.model_cursor = i;
                }
            }
            QcItem::Protocol => {
                self.protocol_cursor = match self.protocol {
                    ProtocolKind::Chat => 0,
                    ProtocolKind::Response => 1,
                };
            }
            QcItem::SessionHeader => {
                self.session_cursor = if self.session_header { 0 } else { 1 };
            }
            QcItem::Retry => {
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
            }
            QcItem::Proxy => {
                // 游标按当前值落位（照抄 Retry 分支）
                self.proxy_cursor = match self.proxy.mode {
                    ProxyMode::Auto => 0,
                    ProxyMode::Off => 1,
                    ProxyMode::Manual => 2,
                };
            }
            _ => {}
        }
    }

    /// 切到某个服务商时带上它自己的模型/Key/窗口/协议/会话头。
    /// 口径照抄 SetupWizard::apply_provider_defaults(防串家)。
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
        let from_env = def
            .as_ref()
            .and_then(|d| d.api_key_env.clone())
            .and_then(|e| std::env::var(e).ok())
            .filter(|v| !v.is_empty());
        self.api_key = plain.or(from_env).unwrap_or_default();
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
        self.key_env_name = if self.key_from_env {
            def.as_ref().and_then(|d| d.api_key_env.clone())
        } else {
            None
        };
        self.model_cursor = 0;
        self.models.clear();
        self.context_window = def.as_ref().and_then(|d| d.context_window);
        self.protocol = def.as_ref().and_then(|d| d.protocol).unwrap_or_default();
        self.protocol_cursor = match self.protocol {
            ProtocolKind::Chat => 0,
            ProtocolKind::Response => 1,
        };
        self.session_header = def.as_ref().map(|d| d.session_header).unwrap_or(false);
        self.session_cursor = if self.session_header { 0 } else { 1 };
    }

    /// 当前形成的 Resolved(未验证;验证后提交的入参)。
    /// 口径照抄 SetupWizard::draft。
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
            context_window: self.context_window,
            protocol: self.protocol,
            session_header_enabled: self.session_header,
            retry: self.retry,
            proxy: self.effective_proxy_for_draft(),
        }
    }

    /// 面板当前三档按 03 §3 落定（env 部分照读，CLI 无；与 resolve_proxy 同优先级，别手写第二份）。
    pub fn effective_proxy_for_draft(&self) -> EffectiveProxy {
        match self.proxy.mode {
            ProxyMode::Off => EffectiveProxy::Direct,
            ProxyMode::Manual => match self
                .proxy
                .url
                .as_deref()
                .map(str::trim)
                .filter(|u| !u.is_empty())
            {
                Some(u) => EffectiveProxy::Via(u.to_string()),
                // 面板 confirm 已拦空串；此处兜底按跟随环境
                None => match znaide_core::update::env_proxy_url() {
                    Some(e) => EffectiveProxy::Via(e),
                    None => EffectiveProxy::Direct,
                },
            },
            ProxyMode::Auto => match znaide_core::update::env_proxy_url() {
                Some(e) => EffectiveProxy::Via(e),
                None => EffectiveProxy::Direct,
            },
        }
    }

    /// Key 脱敏展示:明文只露后 4 位;env 家只显示来源;未设置给提示。
    pub fn key_display(&self) -> String {
        if self.key_from_env {
            match &self.key_env_name {
                Some(n) => format!("来自环境变量 {n}(未落盘)"),
                None => "来自环境变量(未落盘)".to_string(),
            }
        } else if self.api_key.is_empty() {
            "未设置(本地端点可留空)".to_string()
        } else {
            let tail: String = self.api_key.chars().rev().take(4).collect::<String>().chars().rev().collect();
            format!("已设置(****{tail})")
        }
    }

    pub fn current_value(&self, item: QcItem) -> String {
        match item {
            QcItem::Provider => {
                if self.provider.is_empty() {
                    "(未设置)".to_string()
                } else {
                    self.provider.clone()
                }
            }
            QcItem::Model => {
                if self.model.is_empty() {
                    "(未设置)".to_string()
                } else {
                    self.model.clone()
                }
            }
            QcItem::Protocol => self.protocol.as_str().to_string(),
            QcItem::ApiKey => self.key_display(),
            QcItem::BaseUrl => {
                if self.base_url.is_empty() {
                    "(未设置)".to_string()
                } else {
                    self.base_url.clone()
                }
            }
            QcItem::ContextWindow => context_window_text(self.context_window),
            QcItem::MaxTurns => max_turns_text(self.max_turns),
            QcItem::Retry => retry_text(&self.retry),
            QcItem::Proxy => proxy_text(&self.proxy),
            QcItem::SessionHeader => {
                if self.session_header {
                    "开".to_string()
                } else {
                    "关".to_string()
                }
            }
        }
    }

    /// 宿主注入模型查询结果(口径照抄 SetupWizard::inject_models Err 分支:
    /// 失败不卡死,转手输并带出当前模型)。
    pub fn inject_models(&mut self, result: Result<Vec<String>, String>) {
        if let QcStep::Verifying { .. } = self.step {
            return;
        }
        self.typing = false;
        match result {
            Ok(models) if !models.is_empty() => {
                let mut models = models;
                if !self.model.is_empty() && !models.iter().any(|m| *m == self.model) {
                    models.insert(0, self.model.clone());
                }
                let n = models.len();
                self.models = models;
                if let Some(i) = self.models.iter().position(|m| *m == self.model) {
                    self.model_cursor = i;
                }
                self.notice = format!("找到 {n} 个模型:↑↓ 选择,Enter 确认,m 手动输入");
            }
            Ok(_) => {
                self.models.clear();
                self.notice = "该服务商没返回模型列表(可能不支持 /models),按 m 手动输入:".into();
            }
            Err(e) => {
                self.models.clear();
                self.notice = format!("模型列表查询失败: {e}\n按 m 手动输入模型名:");
            }
        }
    }

    /// 验证成功:宿主已保存+生效,面板回菜单并 Notice(05 §6 文案)。
    pub fn inject_verify_ok(&mut self, item: QcItem, detail: String) {
        self.step = QcStep::Menu;
        self.notice = format!("✔ 验证通过:{}已保存并生效。{detail}", item.title());
    }

    /// 验证失败:不落盘不生效,停留编辑态可改可退(02 §4)。
    pub fn inject_verify_err(&mut self, item: QcItem, reason: String) {
        self.step = QcStep::Edit(item);
        self.notice = format!("✘ 验证失败:{reason}。改完按 v 重试,或 Esc 回菜单。");
    }

    /// 直接提交成功:回菜单,连接项追加未验证警告(02 §4)。
    pub fn inject_saved(&mut self, item: QcItem, extra: String) {
        self.step = QcStep::Menu;
        if item.is_local() {
            self.notice = format!("✔ {}已保存并生效。{extra}", item.title());
        } else {
            self.notice = format!(
                "✔ {}已保存并生效。⚠ 未验证连通,建议进该项按 v 跑一次验证。{extra}",
                item.title()
            );
        }
    }

    pub fn paste_text(&mut self, text: &str) -> bool {
        if !self.typing {
            return false;
        }
        self.input_buf
            .extend(text.chars().filter(|c| *c != '\n' && *c != '\r'));
        true
    }

    fn start_verify(&mut self, item: QcItem) -> QuickCfgAction {
        self.step = QcStep::Verifying { item };
        self.progress = format!("正在验证 {} / {} …(Esc 取消)", self.provider, self.model);
        QuickCfgAction::Verify(self.draft())
    }

    fn fetch_models_action(&self) -> QuickCfgAction {
        QuickCfgAction::FetchModels {
            base_url: self.base_url.clone(),
            api_key: if self.api_key.is_empty() {
                None
            } else {
                Some(self.api_key.clone())
            },
            session_header: if self.session_header {
                Some(znaide_core::llm::openai::probe_session_value())
            } else {
                None
            },
            // 验证阶段的模型列表查询同样走待存代理
            proxy: self.effective_proxy_for_draft(),
        }
    }

    /// typing 确认分发(数字三路复用 config_ui::parse_*,端点复用 valid_base_url)。
    fn confirm_typed(&mut self, item: QcItem, v: &str) -> QuickCfgAction {
        if self.typing_custom_url {
            self.typing_custom_url = false;
            if !valid_base_url(v) {
                self.notice = "端点要以 http:// 或 https:// 开头,重新输入:".into();
                self.typing = true;
                self.typing_custom_url = true;
                return QuickCfgAction::None;
            }
            self.provider = "custom".to_string();
            self.base_url = v.trim().to_string();
            self.session_header = false;
            self.session_cursor = 1;
            self.notice = "已切到 custom,建议按 v 验证连通。".into();
            return QuickCfgAction::None;
        }
        if self.typing_context_window {
            self.typing_context_window = false;
            match parse_context_window_input(v) {
                Ok(w) => {
                    self.context_window = w;
                    self.notice = "上下文窗口已更新,按 s 直接提交,或 v 验证后提交。".into();
                }
                Err(msg) => {
                    self.input_buf = v.trim().to_string();
                    self.typing = true;
                    self.typing_context_window = true;
                    self.notice = msg.into();
                }
            }
            return QuickCfgAction::None;
        }
        if self.typing_max_turns {
            self.typing_max_turns = false;
            match parse_max_turns_input(v) {
                Ok(m) => {
                    self.max_turns = m;
                    self.notice = "轮次上限已更新,按 s 直接提交,或 v 验证后提交。".into();
                }
                Err(msg) => {
                    self.input_buf = v.trim().to_string();
                    self.typing = true;
                    self.typing_max_turns = true;
                    self.notice = msg.into();
                }
            }
            return QuickCfgAction::None;
        }
        if self.typing_retry {
            self.typing_retry = false;
            match parse_retry_input(v) {
                Ok(r) => {
                    self.retry = r;
                    self.retry_cursor = if !r.enabled {
                        0
                    } else {
                        match r.max_retries {
                            3 => 1,
                            5 => 2,
                            8 => 3,
                            _ => 4,
                        }
                    };
                    self.notice = "弱网重试已更新,按 s 直接提交,或 v 验证后提交。".into();
                }
                Err(msg) => {
                    self.input_buf = v.trim().to_string();
                    self.typing = true;
                    self.typing_retry = true;
                    self.notice = msg.into();
                }
            }
            return QuickCfgAction::None;
        }
        if self.typing_proxy {
            self.typing_proxy = false;
            // 空输入 → 回 Auto（“清空即回自动”）
            if v.trim().is_empty() {
                self.proxy = proxy_from_cursor(0, None);
                self.proxy_cursor = 0;
                self.notice = "代理已切回跟随环境,按 s 直接提交,或 v 验证后提交。".into();
                return QuickCfgAction::None;
            }
            if !valid_proxy_url(v) {
                // 回填重输（照抄窗口/轮数口径）
                self.input_buf = v.trim().to_string();
                self.typing = true;
                self.typing_proxy = true;
                self.notice = "代理地址要形如 http(s)://host:port 或 socks5(h)://host:port,请重新输入(清空回车 = 跟随环境):".into();
                return QuickCfgAction::None;
            }
            match znaide_core::config::normalize_proxy_url(v) {
                Ok(n) => {
                    self.proxy = ProxyConfig {
                        mode: ProxyMode::Manual,
                        url: Some(n),
                    };
                    self.proxy_cursor = 2;
                    self.notice = "代理已更新,`s` 直存 / `v` 验证后提交。".into();
                }
                Err(e) => {
                    self.input_buf = v.trim().to_string();
                    self.typing = true;
                    self.typing_proxy = true;
                    self.notice = format!("{e:#},请重新输入(清空回车 = 跟随环境):");
                }
            }
            return QuickCfgAction::None;
        }
        match item {
            QcItem::Model => {
                if v.trim().is_empty() {
                    self.notice = "模型名不能为空".into();
                    self.typing = true;
                    return QuickCfgAction::None;
                }
                self.model = v.trim().to_string();
                self.notice = "模型已更新,按 s 直接提交,或 v 验证后提交。".into();
                QuickCfgAction::None
            }
            QcItem::ApiKey => {
                self.api_key = v.to_string();
                self.key_from_env = false;
                self.key_env_name = None;
                self.notice = "key 已更新,按 s 直接提交,或 v 验证后提交。".into();
                QuickCfgAction::None
            }
            QcItem::BaseUrl => {
                if !valid_base_url(v) {
                    self.notice = "端点要以 http:// 或 https:// 开头,重新输入:".into();
                    self.typing = true;
                    return QuickCfgAction::None;
                }
                self.base_url = v.trim().to_string();
                self.notice = "端点已更新,建议按 v 验证后提交。".into();
                QuickCfgAction::None
            }
            _ => QuickCfgAction::None,
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> QuickCfgAction {
        // typing 输入态(各文本/数字输入共用通道)
        if self.typing {
            match key.code {
                KeyCode::Enter => {
                    let item = match self.step {
                        QcStep::Edit(i) => i,
                        _ => QcItem::Model,
                    };
                    let v = std::mem::take(&mut self.input_buf).trim().to_string();
                    self.typing = false;
                    return self.confirm_typed(item, &v);
                }
                KeyCode::Esc => {
                    self.typing = false;
                    self.typing_custom_url = false;
                    self.typing_context_window = false;
                    self.typing_max_turns = false;
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
            return QuickCfgAction::None;
        }

        match self.step {
            QcStep::Menu => match key.code {
                KeyCode::Esc => QuickCfgAction::Exit,
                KeyCode::Up | KeyCode::Char('k') => {
                    self.menu_cursor = self.menu_cursor.saturating_sub(1);
                    QuickCfgAction::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.menu_cursor = (self.menu_cursor + 1).min(QcItem::ALL.len() - 1);
                    QuickCfgAction::None
                }
                KeyCode::Enter | KeyCode::Char('e') => {
                    let item = QcItem::from_index(self.menu_cursor);
                    self.enter_edit(item);
                    if item == QcItem::Model && self.models.is_empty() {
                        return self.fetch_models_action();
                    }
                    QuickCfgAction::None
                }
                KeyCode::Char(c) if ('1'..='9').contains(&c) => {
                    let i = (c as usize) - ('1' as usize);
                    if i < QcItem::ALL.len() {
                        let item = QcItem::from_index(i);
                        self.menu_cursor = i;
                        self.enter_edit(item);
                        if item == QcItem::Model && self.models.is_empty() {
                            return self.fetch_models_action();
                        }
                    }
                    QuickCfgAction::None
                }
                // 第 10 项数字键直达（'1'-'9' 不够用，'0' = index 9）
                KeyCode::Char('0') => {
                    let i = 9;
                    if i < QcItem::ALL.len() {
                        let item = QcItem::from_index(i);
                        self.menu_cursor = i;
                        self.enter_edit(item);
                    }
                    QuickCfgAction::None
                }
                _ => QuickCfgAction::None,
            },
            QcStep::Edit(item) => {
                // 通用双收口(02 §4):s 直接提交,v 验证后提交,Esc 回菜单
                match key.code {
                    KeyCode::Char('s') | KeyCode::Char('S') => {
                        return QuickCfgAction::Save(item);
                    }
                    KeyCode::Char('v') | KeyCode::Char('V') => {
                        return self.start_verify(item);
                    }
                    KeyCode::Esc => {
                        self.step = QcStep::Menu;
                        self.menu_cursor = item.index();
                        return QuickCfgAction::None;
                    }
                    _ => {}
                }
                match item {
                    QcItem::Provider => match key.code {
                        KeyCode::Up | KeyCode::Char('k') => {
                            self.cursor = self.cursor.saturating_sub(1);
                            QuickCfgAction::None
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            self.cursor = (self.cursor + 1)
                                .min(self.providers.len().saturating_sub(1));
                            QuickCfgAction::None
                        }
                        KeyCode::Enter | KeyCode::Char('e') => {
                            if self.providers.is_empty() {
                                return QuickCfgAction::None;
                            }
                            let (name, _, _) =
                                self.providers[self.cursor.min(self.providers.len() - 1)].clone();
                            if name == "custom" {
                                self.typing = true;
                                self.typing_custom_url = true;
                                self.input_buf.clear();
                                self.notice =
                                    "输入自定义端点(base_url),例如 http://localhost:11434/v1:".into();
                                return QuickCfgAction::None;
                            }
                            self.provider = name.clone();
                            self.apply_provider_defaults(&name);
                            self.notice =
                                format!("已切到 {name},建议按 v 验证连通(可在菜单继续微调)。");
                            QuickCfgAction::None
                        }
                        _ => QuickCfgAction::None,
                    },
                    QcItem::Model => match key.code {
                        KeyCode::Up | KeyCode::Char('k') => {
                            self.model_cursor = self.model_cursor.saturating_sub(1);
                            QuickCfgAction::None
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            self.model_cursor = (self.model_cursor + 1)
                                .min(self.models.len().saturating_sub(1));
                            QuickCfgAction::None
                        }
                        KeyCode::Enter | KeyCode::Char('e') => {
                            if !self.models.is_empty() {
                                self.model = self.models
                                    [self.model_cursor.min(self.models.len() - 1)]
                                    .clone();
                                self.notice =
                                    "模型已选中,按 s 直接提交,或 v 验证后提交。".into();
                            }
                            QuickCfgAction::None
                        }
                        KeyCode::Char('m') | KeyCode::Char('M') => {
                            self.typing = true;
                            self.input_buf.clear();
                            self.notice = "手动输入模型名(回车确认):".into();
                            QuickCfgAction::None
                        }
                        _ => QuickCfgAction::None,
                    },
                    QcItem::Protocol => match key.code {
                        KeyCode::Up | KeyCode::Char('k') => {
                            self.protocol_cursor = self.protocol_cursor.saturating_sub(1);
                            self.protocol = [ProtocolKind::Chat, ProtocolKind::Response]
                                [self.protocol_cursor.min(1)];
                            QuickCfgAction::None
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            self.protocol_cursor = (self.protocol_cursor + 1).min(1);
                            self.protocol = [ProtocolKind::Chat, ProtocolKind::Response]
                                [self.protocol_cursor];
                            QuickCfgAction::None
                        }
                        KeyCode::Enter | KeyCode::Char('e') => {
                            self.notice =
                                "协议已选择,按 s 直接提交,或 v 验证后提交。".into();
                            QuickCfgAction::None
                        }
                        _ => QuickCfgAction::None,
                    },
                    QcItem::ApiKey => match key.code {
                        KeyCode::Char('e') | KeyCode::Char('E') | KeyCode::Enter => {
                            self.typing = true;
                            self.input_buf.clear();
                            self.notice = "输入新的 API Key(回车确认,不回显旧值):".into();
                            QuickCfgAction::None
                        }
                        _ => QuickCfgAction::None,
                    },
                    QcItem::BaseUrl => match key.code {
                        KeyCode::Char('e') | KeyCode::Char('E') | KeyCode::Enter => {
                            self.typing = true;
                            self.input_buf = self.base_url.clone();
                            self.notice = "输入新端点(回车确认,须 http(s):// 开头):".into();
                            QuickCfgAction::None
                        }
                        _ => QuickCfgAction::None,
                    },
                    QcItem::ContextWindow => match key.code {
                        KeyCode::Char(c) if c.is_ascii_digit() => {
                            self.typing = true;
                            self.typing_context_window = true;
                            self.input_buf.clear();
                            self.input_buf.push(c);
                            self.notice =
                                "输入上下文窗口 token 数(留空 = 按模型名自动识别):".into();
                            QuickCfgAction::None
                        }
                        KeyCode::Char('t')
                        | KeyCode::Char('T')
                        | KeyCode::Char('e')
                        | KeyCode::Char('E')
                        | KeyCode::Enter => {
                            self.typing = true;
                            self.typing_context_window = true;
                            self.input_buf =
                                self.context_window.map(|v| v.to_string()).unwrap_or_default();
                            self.notice =
                                "输入上下文窗口 token 数(留空 = 按模型名自动识别):".into();
                            QuickCfgAction::None
                        }
                        _ => QuickCfgAction::None,
                    },
                    QcItem::MaxTurns => match key.code {
                        KeyCode::Char(c) if c.is_ascii_digit() => {
                            self.typing = true;
                            self.typing_max_turns = true;
                            self.input_buf.clear();
                            self.input_buf.push(c);
                            self.notice =
                                "输入单条消息最多允许的模型往返轮数(0 = 不限,留空 = 默认):".into();
                            QuickCfgAction::None
                        }
                        KeyCode::Char('t')
                        | KeyCode::Char('T')
                        | KeyCode::Char('e')
                        | KeyCode::Char('E')
                        | KeyCode::Enter => {
                            self.typing = true;
                            self.typing_max_turns = true;
                            self.input_buf =
                                self.max_turns.map(|v| v.to_string()).unwrap_or_default();
                            self.notice =
                                "输入单条消息最多允许的模型往返轮数(0 = 不限,留空 = 默认):".into();
                            QuickCfgAction::None
                        }
                        _ => QuickCfgAction::None,
                    },
                    QcItem::Retry => match key.code {
                        KeyCode::Up | KeyCode::Char('k') => {
                            self.retry_cursor = self.retry_cursor.saturating_sub(1);
                            if let Some(r) = retry_from_cursor(self.retry_cursor) {
                                self.retry = r;
                            }
                            QuickCfgAction::None
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            self.retry_cursor = (self.retry_cursor + 1).min(4);
                            if let Some(r) = retry_from_cursor(self.retry_cursor) {
                                self.retry = r;
                            }
                            QuickCfgAction::None
                        }
                        KeyCode::Char(c) if c.is_ascii_digit() => {
                            self.typing = true;
                            self.typing_retry = true;
                            self.input_buf.clear();
                            self.input_buf.push(c);
                            self.notice =
                                "输入弱网重试次数(空/0 = 关闭,1..=8,超限按 8 算):".into();
                            QuickCfgAction::None
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
                                "输入弱网重试次数(空/0 = 关闭,1..=8,超限按 8 算):".into();
                            QuickCfgAction::None
                        }
                        KeyCode::Enter => {
                            if self.retry_cursor >= 4 {
                                self.typing = true;
                                self.typing_retry = true;
                                self.input_buf = if self.retry.enabled {
                                    self.retry.max_retries.to_string()
                                } else {
                                    String::new()
                                };
                                self.notice =
                                    "输入弱网重试次数(空/0 = 关闭,1..=8,超限按 8 算):".into();
                            } else {
                                self.notice =
                                    "档位已选,按 s 直接提交,或 v 验证后提交。".into();
                            }
                            QuickCfgAction::None
                        }
                        _ => QuickCfgAction::None,
                    },
                    QcItem::SessionHeader => match key.code {
                        KeyCode::Up | KeyCode::Char('k') => {
                            self.session_cursor = self.session_cursor.saturating_sub(1);
                            self.session_header = self.session_cursor == 0;
                            QuickCfgAction::None
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            self.session_cursor = (self.session_cursor + 1).min(1);
                            self.session_header = self.session_cursor == 0;
                            QuickCfgAction::None
                        }
                        KeyCode::Char('y') | KeyCode::Char('Y') => {
                            self.session_cursor = 0;
                            self.session_header = true;
                            self.notice = "会话头:开。按 s 直接提交,或 v 验证后提交。".into();
                            QuickCfgAction::None
                        }
                        KeyCode::Char('n') | KeyCode::Char('N') => {
                            self.session_cursor = 1;
                            self.session_header = false;
                            self.notice = "会话头:关。按 s 直接提交,或 v 验证后提交。".into();
                            QuickCfgAction::None
                        }
                        KeyCode::Enter | KeyCode::Char('e') => {
                            self.notice = "会话头已选择,按 s 直接提交,或 v 验证后提交。".into();
                            QuickCfgAction::None
                        }
                        _ => QuickCfgAction::None,
                    },
                    QcItem::Proxy => match key.code {
                        KeyCode::Up | KeyCode::Char('k') => {
                            self.proxy_cursor = self.proxy_cursor.saturating_sub(1);
                            self.proxy =
                                proxy_from_cursor(self.proxy_cursor, self.proxy.url.clone());
                            QuickCfgAction::None
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            self.proxy_cursor = (self.proxy_cursor + 1).min(2);
                            self.proxy =
                                proxy_from_cursor(self.proxy_cursor, self.proxy.url.clone());
                            QuickCfgAction::None
                        }
                        // y = 跟随环境（follow），n = 直连（no proxy）
                        KeyCode::Char('y') | KeyCode::Char('Y') => {
                            self.proxy_cursor = 0;
                            self.proxy = proxy_from_cursor(0, None);
                            self.notice = "代理:跟随环境。按 s 直接提交,或 v 验证后提交。".into();
                            QuickCfgAction::None
                        }
                        KeyCode::Char('n') | KeyCode::Char('N') => {
                            self.proxy_cursor = 1;
                            self.proxy = proxy_from_cursor(1, None);
                            self.notice =
                                "代理:直连(忽略环境代理)。按 s 直接提交,或 v 验证后提交。".into();
                            QuickCfgAction::None
                        }
                        KeyCode::Char(c) if c.is_ascii_digit() => {
                            self.typing = true;
                            self.typing_proxy = true;
                            self.input_buf.clear();
                            self.input_buf.push(c);
                            self.notice =
                                "输入代理地址(形如 http://127.0.0.1:10808，清空回车 = 跟随环境):"
                                    .into();
                            QuickCfgAction::None
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
                                "输入代理地址(形如 http://127.0.0.1:10808，清空回车 = 跟随环境):"
                                    .into();
                            QuickCfgAction::None
                        }
                        _ => QuickCfgAction::None,
                    },
                }
            }
            QcStep::Verifying { item } => {
                if matches!(key.code, KeyCode::Esc) {
                    self.step = QcStep::Edit(item);
                    self.progress.clear();
                }
                QuickCfgAction::None
            }
        }
    }
}

/// 渲染面板(标题与全向导区分:按需配置 vs 配置向导)。
impl QuickCfgPanel {
    pub fn render(&self, f: &mut ratatui::Frame<'_>, area: Rect) {
        f.render_widget(Clear, area);
        let block = Block::default()
            .title(" 按需配置 — 选一项改一项(/config 走全量向导) ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan));
        let inner = block.inner(area);
        f.render_widget(block, area);

        let mut lines: Vec<Line> = Vec::new();
        // 顶部摘要行(与全向导同样式)
        lines.push(Line::from(vec![Span::styled(
            format!(
                "当前: {} / {} / {} · 轮数{} · 重试{} · 窗口{} · 代理{}",
                if self.provider.is_empty() {
                    "?"
                } else {
                    &self.provider
                },
                if self.model.is_empty() {
                    "?"
                } else {
                    &self.model
                },
                if self.base_url.is_empty() {
                    "?"
                } else {
                    &self.base_url
                },
                max_turns_text(self.max_turns),
                retry_text(&self.retry),
                context_window_text(self.context_window),
                proxy_text_short(&self.proxy),
            ),
            Style::default().fg(Color::DarkGray),
        )]));
        lines.push(Line::from(""));

        match self.step {
            QcStep::Menu => {
                for (i, item) in QcItem::ALL.iter().enumerate() {
                    let cur = if i == self.menu_cursor { ">" } else { " " };
                    let style = if i == self.menu_cursor {
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    lines.push(Line::from(vec![Span::styled(
                        format!(
                            "{cur} {}. {}: {} {}",
                            // 第 10 项数字键是 '0'（'1'-'9' 不够用），显示与按键一致
                            if i == 9 {
                                "0".to_string()
                            } else {
                                (i + 1).to_string()
                            },
                            item.title(),
                            self.current_value(*item),
                            item.tag()
                        ),
                        style,
                    )]));
                }
                lines.push(Line::from(""));
                lines.push(Line::from(vec![Span::styled(
                    "↑↓ 选择 · Enter 编辑 · 1-9,0 直达 · Esc 退出",
                    Style::default().fg(Color::DarkGray),
                )]));
            }
            QcStep::Edit(item) => {
                lines.push(Line::from(vec![Span::styled(
                    format!("{} {}", item.title(), item.tag()),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )]));
                lines.push(Line::from(vec![Span::styled(
                    format!("当前值: {}", self.current_value(item)),
                    Style::default().fg(Color::DarkGray),
                )]));
                lines.push(Line::from(""));
                match item {
                    QcItem::Provider => {
                        for (i, (n, base, _)) in self.providers.iter().enumerate() {
                            let cur = if i == self.cursor { ">" } else { " " };
                            let style = if i == self.cursor {
                                Style::default().fg(Color::Black).bg(Color::Cyan)
                            } else {
                                Style::default()
                            };
                            lines.push(Line::from(vec![Span::styled(
                                format!("{cur} {n}  {base}"),
                                style,
                            )]));
                        }
                        lines.push(Line::from(vec![Span::styled(
                            "Enter 选中切家(停留本屏,可继续微调) · Esc 回菜单",
                            Style::default().fg(Color::DarkGray),
                        )]));
                    }
                    QcItem::Model => {
                        if self.models.is_empty() {
                            lines.push(Line::from("模型列表查询中/不可用,按 m 手动输入。"));
                        } else {
                            let n = self.models.len().min(25);
                            let first = self
                                .model_cursor
                                .saturating_sub(12)
                                .min(self.models.len().saturating_sub(n));
                            for (k, m) in self.models[first..(first + n).min(self.models.len())]
                                .iter()
                                .enumerate()
                            {
                                let i = first + k;
                                let cur = if i == self.model_cursor { ">" } else { " " };
                                let style = if i == self.model_cursor {
                                    Style::default().fg(Color::Black).bg(Color::Cyan)
                                } else {
                                    Style::default()
                                };
                                lines.push(Line::from(vec![Span::styled(
                                    format!("{cur} {m}"),
                                    style,
                                )]));
                            }
                            lines.push(Line::from(vec![Span::styled(
                                "↑↓ 选择 · Enter 确认 · m 手动输入",
                                Style::default().fg(Color::DarkGray),
                            )]));
                        }
                    }
                    QcItem::Protocol => {
                        for (i, name) in ["chat", "response"].iter().enumerate() {
                            let cur = if i == self.protocol_cursor { ">" } else { " " };
                            let style = if i == self.protocol_cursor {
                                Style::default().fg(Color::Black).bg(Color::Cyan)
                            } else {
                                Style::default()
                            };
                            lines.push(Line::from(vec![Span::styled(
                                format!("{cur} {name}"),
                                style,
                            )]));
                        }
                        lines.push(Line::from(vec![Span::styled(
                            "↑↓ 选择 · Enter 确认",
                            Style::default().fg(Color::DarkGray),
                        )]));
                    }
                    QcItem::ApiKey => {
                        lines.push(Line::from("Key 不回显旧值,只显示来源(见当前值行)。"));
                        lines.push(Line::from(vec![Span::styled(
                            "e/Enter 编辑(覆盖) · Esc 放弃",
                            Style::default().fg(Color::DarkGray),
                        )]));
                    }
                    QcItem::BaseUrl => {
                        lines.push(Line::from("端点须 http(s):// 开头。"));
                        lines.push(Line::from(vec![Span::styled(
                            "e/Enter 编辑 · Esc 放弃",
                            Style::default().fg(Color::DarkGray),
                        )]));
                    }
                    QcItem::ContextWindow => {
                        lines.push(Line::from("留空 = 自动(按模型名匹配内置表)。"));
                        lines.push(Line::from(vec![Span::styled(
                            "直接敲数字 / t 编辑 · Enter 仅聚焦输入",
                            Style::default().fg(Color::DarkGray),
                        )]));
                    }
                    QcItem::MaxTurns => {
                        lines.push(Line::from("0 = 不限,留空 = 默认。"));
                        lines.push(Line::from(vec![Span::styled(
                            "直接敲数字 / t 编辑 · Enter 仅聚焦输入",
                            Style::default().fg(Color::DarkGray),
                        )]));
                    }
                    QcItem::Retry => {
                        for (i, name) in
                            ["关闭", "3 次", "5 次", "8 次", "手动输入"].iter().enumerate()
                        {
                            let cur = if i == self.retry_cursor { ">" } else { " " };
                            let style = if i == self.retry_cursor {
                                Style::default().fg(Color::Black).bg(Color::Cyan)
                            } else {
                                Style::default()
                            };
                            lines.push(Line::from(vec![Span::styled(
                                format!("{cur} {name}"),
                                style,
                            )]));
                        }
                        lines.push(Line::from(vec![Span::styled(
                            "↑↓ 档位 · 数字/t 手动 · Enter 手动档进输入",
                            Style::default().fg(Color::DarkGray),
                        )]));
                    }
                    QcItem::SessionHeader => {
                        for (i, name) in ["是(开)", "否(关)"].iter().enumerate() {
                            let cur = if i == self.session_cursor { ">" } else { " " };
                            let style = if i == self.session_cursor {
                                Style::default().fg(Color::Black).bg(Color::Cyan)
                            } else {
                                Style::default()
                            };
                            lines.push(Line::from(vec![Span::styled(
                                format!("{cur} {name}"),
                                style,
                            )]));
                        }
                        lines.push(Line::from(vec![Span::styled(
                            "y/n 直达 · ↑↓+Enter 确认",
                            Style::default().fg(Color::DarkGray),
                        )]));
                    }
                    QcItem::Proxy => {
                        lines.push(Line::from(
                            "全局出口：模型请求 / 网页抓取 / 更新检查统一走这里。本地 ollama 等走 NO_PROXY 豁免，不受影响。",
                        ));
                        for (i, name) in ["跟随环境(默认)", "直连(忽略环境代理)", "手动指定…"]
                            .iter()
                            .enumerate()
                        {
                            let cur = if i == self.proxy_cursor { ">" } else { " " };
                            let style = if i == self.proxy_cursor {
                                Style::default().fg(Color::Black).bg(Color::Cyan)
                            } else {
                                Style::default()
                            };
                            lines.push(Line::from(vec![Span::styled(
                                format!("{cur} {name}"),
                                style,
                            )]));
                        }
                        lines.push(Line::from(vec![Span::styled(
                            "↑↓ 档位 · y 跟随环境 / n 直连 · 数字/t/e/Enter 手输地址",
                            Style::default().fg(Color::DarkGray),
                        )]));
                    }
                }
                if self.typing {
                    lines.push(Line::from(vec![
                        Span::styled("输入: ", Style::default().fg(Color::Yellow)),
                        Span::styled(
                            format!("{}▊", self.input_buf),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                    ]));
                }
                lines.push(Line::from(""));
                let def_mark = match item.default_submit() {
                    SubmitKind::Direct => "s 直接提交(默认) · v 验证后提交 · Esc 回菜单(编辑丢弃)",
                    SubmitKind::Verified => "s 直接提交 · v 验证后提交(默认) · Esc 回菜单(编辑丢弃)",
                };
                lines.push(Line::from(vec![Span::styled(
                    def_mark,
                    Style::default().fg(Color::Yellow),
                )]));
            }
            QcStep::Verifying { .. } => {
                let msg = if self.progress.is_empty() {
                    "正在验证…(Esc 取消)".to_string()
                } else {
                    format!("{} (Esc 取消)", self.progress)
                };
                lines.push(Line::from(vec![Span::styled(
                    msg,
                    Style::default().fg(Color::Yellow),
                )]));
            }
        }
        if !self.notice.is_empty() {
            lines.push(Line::from(""));
            for l in self.notice.lines() {
                lines.push(Line::from(vec![Span::styled(
                    l.to_string(),
                    Style::default().fg(Color::Yellow),
                )]));
            }
        }
        let p = Paragraph::new(lines)
            .block(Block::default())
            .style(Style::default());
        f.render_widget(p, inner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn panel_with_cfg() -> QuickCfgPanel {
        let mut p = QuickCfgPanel::new();
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "ollama".into(),
            ProviderDef {
                base_url: Some("http://localhost:11434/v1".into()),
                model: Some("qwen3:8b".into()),
                ..Default::default()
            },
        );
        let cfg = Config {
            provider: Some("ollama".into()),
            max_turns: Some(300),
            retry: RetryConfig::enabled_with(5),
            providers,
            ..Default::default()
        };
        p.apply_config(&cfg);
        p
    }

    /// need03 U7:别名表全命中;未知别名→None(宿主落菜单+Notice)
    #[test]
    fn alias_table_hits() {
        assert_eq!(parse_qc_alias("retry"), Some(QcItem::Retry));
        assert_eq!(parse_qc_alias("重试"), Some(QcItem::Retry));
        assert_eq!(parse_qc_alias("window"), Some(QcItem::ContextWindow));
        assert_eq!(parse_qc_alias("CTX"), Some(QcItem::ContextWindow));
        assert_eq!(parse_qc_alias("上下文"), Some(QcItem::ContextWindow));
        assert_eq!(parse_qc_alias("turns"), Some(QcItem::MaxTurns));
        assert_eq!(parse_qc_alias("max-turns"), Some(QcItem::MaxTurns));
        assert_eq!(parse_qc_alias("MAX_TURNS"), Some(QcItem::MaxTurns));
        assert_eq!(parse_qc_alias("轮次"), Some(QcItem::MaxTurns));
        assert_eq!(parse_qc_alias("key"), Some(QcItem::ApiKey));
        assert_eq!(parse_qc_alias("API-KEY"), Some(QcItem::ApiKey));
        assert_eq!(parse_qc_alias("model"), Some(QcItem::Model));
        assert_eq!(parse_qc_alias("模型"), Some(QcItem::Model));
        assert_eq!(parse_qc_alias("url"), Some(QcItem::BaseUrl));
        assert_eq!(parse_qc_alias("base_url"), Some(QcItem::BaseUrl));
        assert_eq!(parse_qc_alias("端点"), Some(QcItem::BaseUrl));
        assert_eq!(parse_qc_alias("protocol"), Some(QcItem::Protocol));
        assert_eq!(parse_qc_alias("协议"), Some(QcItem::Protocol));
        assert_eq!(parse_qc_alias("provider"), Some(QcItem::Provider));
        assert_eq!(parse_qc_alias("服务商"), Some(QcItem::Provider));
        assert_eq!(
            parse_qc_alias("session_header"),
            Some(QcItem::SessionHeader)
        );
        assert_eq!(parse_qc_alias("会话头"), Some(QcItem::SessionHeader));
        assert_eq!(parse_qc_alias("乱七八糟"), None);
        assert_eq!(parse_qc_alias(""), None);
    }

    /// need03 U8:代理别名命中
    #[test]
    fn proxy_alias_hits() {
        assert_eq!(parse_qc_alias("proxy"), Some(QcItem::Proxy));
        assert_eq!(parse_qc_alias("代理"), Some(QcItem::Proxy));
        assert_eq!(parse_qc_alias("网络代理"), Some(QcItem::Proxy));
        assert_eq!(parse_qc_alias("network_proxy"), Some(QcItem::Proxy));
        assert_eq!(parse_qc_alias("PROXY"), Some(QcItem::Proxy));
    }

    /// need03 U8:数字三路(留空/0/非法/超限)与 confirm_typed 同语义
    #[test]
    fn number_parsing_matches_wizard() {
        assert_eq!(parse_context_window_input(""), Ok(None));
        assert_eq!(parse_context_window_input("  "), Ok(None));
        assert_eq!(parse_context_window_input("128000"), Ok(Some(128000)));
        assert!(parse_context_window_input("0").is_err());
        assert!(parse_context_window_input("12x").is_err());
        assert_eq!(parse_max_turns_input(""), Ok(None));
        assert_eq!(parse_max_turns_input("0"), Ok(Some(0)));
        assert_eq!(parse_max_turns_input("500"), Ok(Some(500)));
        assert!(parse_max_turns_input("abc").is_err());
        assert_eq!(parse_retry_input(""), Ok(RetryConfig::disabled()));
        assert_eq!(parse_retry_input("0"), Ok(RetryConfig::disabled()));
        assert_eq!(
            parse_retry_input("5"),
            Ok(RetryConfig::enabled_with(5))
        );
        assert_eq!(
            parse_retry_input("99"),
            Ok(RetryConfig::enabled_with(
                znaide_core::config::MAX_RETRIES
            ))
        );
        assert!(parse_retry_input("x").is_err());
        assert!(!valid_base_url("ftp://x"));
        assert!(valid_base_url("https://api.example.com/v1"));
    }

    /// need03 U9:key_from_env 守卫(不预填明文;手输后翻转)
    #[test]
    fn key_from_env_guard() {
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "dashscope".into(),
            ProviderDef {
                base_url: Some("https://dashscope.aliyuncs.com/compatible-mode/v1".into()),
                model: Some("qwen-plus".into()),
                api_key_env: Some("DASHSCOPE_API_KEY".into()),
                ..Default::default()
            },
        );
        let cfg = Config {
            provider: Some("dashscope".into()),
            providers,
            ..Default::default()
        };
        let mut p = QuickCfgPanel::new();
        p.apply_config(&cfg);
        assert!(p.key_from_env);
        assert!(p.api_key.is_empty());
        assert!(p.key_display().contains("DASHSCOPE_API_KEY"));
        // 手输后翻转:保存会写明文
        p.step = QcStep::Edit(QcItem::ApiKey);
        p.typing = true;
        p.on_key(key(KeyCode::Char('s')));
        p.on_key(key(KeyCode::Char('k')));
        p.on_key(key(KeyCode::Enter));
        assert!(!p.key_from_env);
        assert_eq!(p.api_key, "sk");
    }

    /// need03 U10:默认收口矩阵(C1–C5+代理 验证,C6–C9 直接)
    #[test]
    fn default_submit_matrix() {
        for i in [
            QcItem::Provider,
            QcItem::Model,
            QcItem::Protocol,
            QcItem::ApiKey,
            QcItem::BaseUrl,
            // 代理填错 = 全网不通，必须验证（与 BaseUrl 同档）
            QcItem::Proxy,
        ] {
            assert_eq!(i.default_submit(), SubmitKind::Verified, "{:?}", i);
        }
        for i in [
            QcItem::ContextWindow,
            QcItem::MaxTurns,
            QcItem::Retry,
            QcItem::SessionHeader,
        ] {
            assert_eq!(i.default_submit(), SubmitKind::Direct, "{:?}", i);
        }
    }

    /// 菜单分发:回车进编辑,数字直达,Esc 退出;s/v 双收口
    #[test]
    fn menu_dispatch_and_submit() {
        let mut p = panel_with_cfg();
        assert_eq!(p.step, QcStep::Menu);
        // 数字直达:8→重试
        match p.on_key(key(KeyCode::Char('8'))) {
            QuickCfgAction::None => {}
            _ => panic!("数字应进编辑"),
        }
        assert_eq!(p.step, QcStep::Edit(QcItem::Retry));
        // Esc 回菜单
        match p.on_key(key(KeyCode::Esc)) {
            QuickCfgAction::None => {}
            _ => panic!("Esc 应回菜单"),
        }
        assert_eq!(p.step, QcStep::Menu);
        // 回车进游标项
        p.menu_cursor = QcItem::MaxTurns.index();
        match p.on_key(key(KeyCode::Enter)) {
            QuickCfgAction::None => {}
            _ => panic!("Enter 应进编辑"),
        }
        assert_eq!(p.step, QcStep::Edit(QcItem::MaxTurns));
        // s 直接提交
        match p.on_key(key(KeyCode::Char('s'))) {
            QuickCfgAction::Save(QcItem::MaxTurns) => {}
            _ => panic!("s 应直接提交"),
        }
        // v 验证后提交(进 Verifying)
        let mut p2 = panel_with_cfg();
        p2.step = QcStep::Edit(QcItem::Model);
        match p2.on_key(key(KeyCode::Char('v'))) {
            QuickCfgAction::Verify(_) => {}
            _ => panic!("v 应验证"),
        }
        assert!(matches!(p2.step, QcStep::Verifying { .. }));
        // Verifying 中 Esc 取消回编辑
        match p2.on_key(key(KeyCode::Esc)) {
            QuickCfgAction::None => {}
            _ => panic!("Esc 应取消"),
        }
        assert_eq!(p2.step, QcStep::Edit(QcItem::Model));
        // Menu 中 Esc 退出
        let mut p3 = panel_with_cfg();
        match p3.on_key(key(KeyCode::Esc)) {
            QuickCfgAction::Exit => {}
            _ => panic!("Menu Esc 应退出"),
        }
    }

    /// 重试档位游标即时落值;手动输入超限夹取
    #[test]
    fn retry_cursor_and_manual() {
        let mut p = panel_with_cfg();
        p.step = QcStep::Edit(QcItem::Retry);
        p.retry_cursor = 0;
        p.retry = RetryConfig::disabled();
        p.on_key(key(KeyCode::Down));
        assert_eq!(p.retry, RetryConfig::enabled_with(3));
        // 手动输入 99→夹取 8
        p.retry_cursor = 4;
        p.on_key(key(KeyCode::Enter));
        assert!(p.typing && p.typing_retry);
        for c in "99".chars() {
            p.on_key(key(KeyCode::Char(c)));
        }
        p.on_key(key(KeyCode::Enter));
        assert_eq!(
            p.retry.max_retries,
            znaide_core::config::MAX_RETRIES
        );
    }

    /// 切家带出新家字段(防串家)
    #[test]
    fn switch_provider_isolated() {
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "houseA".into(),
            ProviderDef {
                base_url: Some("http://a/v1".into()),
                model: Some("model-a".into()),
                api_key: Some("sk-a".into()),
                ..Default::default()
            },
        );
        providers.insert(
            "houseB".into(),
            ProviderDef {
                base_url: Some("http://b/v1".into()),
                model: Some("model-b".into()),
                ..Default::default()
            },
        );
        let cfg = Config {
            provider: Some("houseA".into()),
            providers,
            ..Default::default()
        };
        let mut p = QuickCfgPanel::new();
        p.providers = vec![
            ("houseA".into(), "http://a/v1".into(), "model-a".into()),
            ("houseB".into(), "http://b/v1".into(), "model-b".into()),
        ];
        p.apply_config(&cfg);
        assert_eq!(p.model, "model-a");
        p.step = QcStep::Edit(QcItem::Provider);
        p.cursor = 1;
        p.on_key(key(KeyCode::Enter));
        assert_eq!(p.provider, "houseB");
        assert_eq!(p.model, "model-b");
        assert!(p.api_key.is_empty());
    }

    /// need03 U9:代理档位键（↑↓切三档即时落值；y→Auto/n→Off；Enter进手输；
    /// 非法URL回填重输；空输入回Auto；数字键0直达第10项）
    #[test]
    fn proxy_cursor_keys_and_manual() {
        let mut p = panel_with_cfg();
        // 数字键 0 直达第 10 项
        match p.on_key(key(KeyCode::Char('0'))) {
            QuickCfgAction::None => {}
            _ => panic!("0 应直达第 10 项"),
        }
        assert_eq!(p.step, QcStep::Edit(QcItem::Proxy));
        // ↓ 切到直连即时落值
        p.on_key(key(KeyCode::Down));
        assert_eq!(p.proxy_cursor, 1);
        assert_eq!(p.proxy.mode, ProxyMode::Off);
        // ↓ 切到手动（保持原 url）
        p.on_key(key(KeyCode::Down));
        assert_eq!(p.proxy_cursor, 2);
        assert_eq!(p.proxy.mode, ProxyMode::Manual);
        // y 回跟随环境
        p.on_key(key(KeyCode::Char('y')));
        assert_eq!(p.proxy.mode, ProxyMode::Auto);
        assert_eq!(p.proxy_cursor, 0);
        // n 到直连
        p.on_key(key(KeyCode::Char('n')));
        assert_eq!(p.proxy.mode, ProxyMode::Off);
        // Enter 进手输
        p.on_key(key(KeyCode::Enter));
        assert!(p.typing && p.typing_proxy);
        // 非法 URL 回填重输（仍在输入态）
        for c in "ftp://bad".chars() {
            p.on_key(key(KeyCode::Char(c)));
        }
        p.on_key(key(KeyCode::Enter));
        assert!(p.typing && p.typing_proxy, "非法地址应回填重输");
        assert_eq!(p.input_buf, "ftp://bad");
        // Esc 退出输入态
        p.on_key(key(KeyCode::Esc));
        assert!(!p.typing);
        // 合法地址进 Manual
        p.on_key(key(KeyCode::Enter));
        for c in "http://127.0.0.1:10808/".chars() {
            p.on_key(key(KeyCode::Char(c)));
        }
        // input_buf 预填了现存 url（空），上面逐字追加后确认
        p.on_key(key(KeyCode::Enter));
        assert_eq!(p.proxy.mode, ProxyMode::Manual);
        assert_eq!(p.proxy.url.as_deref(), Some("http://127.0.0.1:10808"));
        assert_eq!(p.proxy_cursor, 2);
        // 空输入回 Auto（进手输后清空预填再确认）
        p.on_key(key(KeyCode::Enter));
        assert!(p.typing && p.typing_proxy);
        p.input_buf.clear();
        p.on_key(key(KeyCode::Enter));
        assert_eq!(p.proxy.mode, ProxyMode::Auto);
    }
}
