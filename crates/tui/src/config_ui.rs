//! 配置向导状态机(服务商 → 模型 → Key → 轮数上限 → 验证)。
//! 与 app.rs 解耦:on_key 返回 WizardAction,宿主执行异步动作(查询模型/验证),
//! 再把结果经 inject_models / inject_verify 回填。
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use znaide_core::config::Config;
use znaide_core::config::Resolved;

/// 向导步骤
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// 选择服务商(列表)
    Provider,
    /// 正在查询模型(等待宿主回调)
    Querying,
    /// 模型列表(可上下选,或手动输入)
    ModelSelect,
    /// 输入 API Key
    ApiKey,
    /// 轮数上限(单条消息最多几轮模型往返;0 = 不限)
    MaxTurns,
    /// 正在验证(等待宿主回调)
    Verifying,
}

/// 宿主需要执行的异步动作
pub enum WizardAction {
    None,
    /// 查询模型列表(参数:base_url + api_key,由宿主调用 OpenAiClient)
    FetchModels { base_url: String, api_key: Option<String> },
    /// 验证连接(base_url/model/api_key,由宿主调用 client.validate)
    Verify(Resolved),
    /// 保存配置(验证通过后)
    Save(Resolved),
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
    /// 当前 typing 输入的是 max_turns(与模型名/key/端点共用输入通道,靠它区分)
    pub typing_max_turns: bool,
    /// Key 来自环境变量(api_key_env):面板里显示为空但实际可用
    pub key_from_env: bool,
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
            typing_max_turns: false,
            key_from_env: false,
        }
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
        let r = cfg.resolve(None, None, None, None).ok();
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
        self.api_key = cfg
            .api_key
            .clone()
            .filter(|k| !k.is_empty())
            .or_else(|| {
                def.as_ref()
                    .and_then(|d| d.api_key.clone())
                    .filter(|k| !k.is_empty())
            })
            .unwrap_or_default();
        self.key_from_env =
            self.api_key.is_empty() && def.as_ref().map(|d| d.api_key_env.is_some()).unwrap_or(false);
        self.max_turns = cfg.max_turns;
    }

    /// 是否已有配置可展示(首次运行全空 → 不显示摘要行)
    pub fn has_configured(&self) -> bool {
        !self.provider.is_empty() || !self.model.is_empty() || !self.api_key.is_empty()
    }

    /// 轮数上限的展示文案
    pub fn max_turns_label(&self) -> String {
        match self.max_turns {
            None => format!("默认({} 轮)", znaide_core::session::DEFAULT_MAX_TURNS),
            Some(0) => "不限".to_string(),
            Some(n) => format!("{n} 轮"),
        }
    }

    pub fn is_idle_step(&self) -> bool {
        matches!(
            self.step,
            Step::Provider | Step::ModelSelect | Step::ApiKey | Step::MaxTurns
        )
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
            context_window: None,
        }
    }

    /// 宿主注入模型查询结果
    pub fn inject_models(&mut self, result: Result<Vec<String>, String>) {
        self.step = Step::ModelSelect;
        self.typing = false;
        match result {
            Ok(models) if !models.is_empty() => {
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

    /// 进入"正在查询模型"状态(宿主开始查询前调用)
    fn start_querying(&mut self, base_url: &str, api_key: Option<&str>) -> WizardAction {
        self.step = Step::Querying;
        self.progress = format!("正在查询 {base_url} 的模型列表…");
        self.base_url = base_url.to_string();
        self.api_key = api_key.unwrap_or("").to_string();
        WizardAction::FetchModels {
            base_url: base_url.to_string(),
            api_key: api_key.map(|s| s.to_string()),
        }
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
                        self.api_key = env_key_for(&name);
                        self.key_from_env = self.api_key.is_empty();
                    }
                    if name == "custom" {
                        // 自定义服务商:先输入端点
                        self.typing = true;
                        self.input_buf.clear();
                        self.notice = "输入自定义端点(base_url),例如 http://localhost:11434/v1 或 https://api.example.com/v1:".into();
                        return WizardAction::None;
                    }
                    let key = if self.api_key.is_empty() { None } else { Some(self.api_key.clone()) };
                    // 尝试模型自动查询;失败可手输
                    self.start_querying(&base, key.as_deref())
                }
                _ => WizardAction::None,
            },
            Step::Querying => {
                // 等待宿主;按 Esc 放弃回到服务商
                if matches!(key.code, KeyCode::Esc) {
                    self.step = Step::Provider;
                }
                WizardAction::None
            }
            Step::ModelSelect => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.model_cursor = self.model_cursor.saturating_sub(1);
                    WizardAction::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.model_cursor = (self.model_cursor + 1).min(self.models.len().saturating_sub(1));
                    WizardAction::None
                }
                KeyCode::Enter | KeyCode::Char('e') => {
                    if !self.models.is_empty() {
                        self.model = self.models[self.model_cursor.min(self.models.len() - 1)].clone();
                    }
                    self.step = Step::ApiKey;
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
            Step::ApiKey => match key.code {
                KeyCode::Esc => {
                    // 回退:直接回模型选择(保留模型);若没有模型就回服务商
                    self.step = if self.models.is_empty() {
                        Step::Provider
                    } else {
                        Step::ModelSelect
                    };
                    WizardAction::None
                }
                KeyCode::Char('s') | KeyCode::Char('S') => {
                    // 验证(跳过轮数上限那步,直接用当前值)
                    self.step = Step::Verifying;
                    self.progress = format!("正在验证 {} / {} …", self.provider, self.model);
                    WizardAction::Verify(self.draft())
                }
                KeyCode::Enter => {
                    // 下一步:轮数上限
                    self.step = Step::MaxTurns;
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
            Step::MaxTurns => match key.code {
                // 直接敲数字就进输入(照着屏幕上显示的改最直观)
                KeyCode::Char(c) if c.is_ascii_digit() => {
                    self.typing = true;
                    self.typing_max_turns = true;
                    self.input_buf.clear();
                    self.input_buf.push(c);
                    self.notice = "输入单条消息最多允许的模型往返轮数(0 = 不限,留空 = 默认):".into();
                    WizardAction::None
                }
                KeyCode::Char('t') | KeyCode::Char('T') | KeyCode::Char('e') | KeyCode::Char('E') => {
                    self.typing = true;
                    self.typing_max_turns = true;
                    self.input_buf = self.max_turns.map(|v| v.to_string()).unwrap_or_default();
                    self.notice = "输入单条消息最多允许的模型往返轮数(0 = 不限,留空 = 默认):".into();
                    WizardAction::None
                }
                KeyCode::Enter | KeyCode::Char('s') | KeyCode::Char('S') => {
                    // 完成:验证(用当前轮数上限)
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
            Step::Verifying => {
                // 等待宿主;按 Esc 回到上一步(轮数上限)
                if matches!(key.code, KeyCode::Esc) {
                    self.step = Step::MaxTurns;
                }
                WizardAction::None
            }
        }
    }

    /// 处理输入框提交的内容(取决于当前阶段)
    fn confirm_typed(&mut self, v: &str) -> WizardAction {
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
        match self.step {
            // 自定义服务商:提交的是端点,随后查询模型
            Step::Provider if self.provider == "custom" => {
                if v.is_empty() || !(v.starts_with("http://") || v.starts_with("https://")) {
                    self.notice = "端点要以 http:// 或 https:// 开头,重新输入:".into();
                    self.typing = true;
                    return WizardAction::None;
                }
                let key = if self.api_key.is_empty() { None } else { Some(self.api_key.clone()) };
                self.start_querying(v, key.as_deref())
            }
            // 手动输入模型(查询失败或按 m)
            Step::ModelSelect => {
                if v.is_empty() {
                    self.notice = "模型名不能为空".into();
                    self.typing = true;
                    return WizardAction::None;
                }
                self.model = v.to_string();
                self.step = Step::ApiKey;
                WizardAction::None
            }
            // 编辑 key(从 ApiKey 进入输入后,仍在 ApiKey 状态,typing 模式)
            _ => {
                self.api_key = v.to_string();
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
        let steps = ["1 服务商", "2 模型", "3 API Key", "4 轮数上限", "5 验证"];
        let current = match self.step {
            Step::Provider => 0,
            Step::Querying | Step::ModelSelect => 1,
            Step::ApiKey => 2,
            Step::MaxTurns => 3,
            Step::Verifying => 4,
        };
        let mut bar: Vec<Span> = Vec::new();
        for (i, s) in steps.iter().enumerate() {
            let st = if i == current {
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
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
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(
                        "  端点 {}  Key {}  轮数 {}",
                        self.base_url,
                        key,
                        self.max_turns_label()
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
                        Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    };
                    let default_model = if m.is_empty() { String::new() } else { format!("(默认 {m})") };
                    lines.push(Line::from(vec![
                        Span::styled(marker, Style::default().fg(Color::Cyan)),
                        Span::styled(name.clone(), st),
                        Span::styled(format!("  {base} {default_model}"), Style::default().fg(Color::DarkGray)),
                    ]));
                }
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
                    for (i, m) in self.models.iter().enumerate().take(25) {
                        let sel = i == self.model_cursor;
                        let marker = if sel { "▶ " } else { "  " };
                        let st = if sel {
                            Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(Color::DarkGray)
                        };
                        lines.push(Line::from(vec![
                            Span::styled(marker, Style::default().fg(Color::Cyan)),
                            Span::styled(m.clone(), st),
                        ]));
                    }
                    if self.models.len() > 25 {
                        lines.push(Line::from(Span::styled(
                            format!("…(共 {} 个,可输入筛选)", self.models.len()),
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
            Step::ApiKey => {
                lines.push(Line::from(Span::styled(
                    format!("服务商: {} | 模型: {}", self.provider, self.model),
                    Style::default().fg(Color::Green),
                )));
                let key_disp = if self.api_key.is_empty() {
                    if self.key_from_env {
                        "(来自环境变量,留空即保持)".to_string()
                    } else {
                        "(未设置)".to_string()
                    }
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
                lines.push(Line::from(Span::styled(
                    "e 输入 key(本地服务可留空)| Enter 下一步(轮数上限)| s 直接验证并完成 | Esc 返回改模型",
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
                        Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                    ),
                ]));
                lines.push(Line::from(Span::styled(
                    "单条消息最多允许的模型往返轮数(一轮可含多次工具调用);用完会问你要不要再放一批",
                    Style::default().fg(Color::DarkGray),
                )));
                lines.push(Line::from(Span::styled(
                    "直接输入数字修改(0 = 不限,留空 = 默认)| Enter 或 s 验证并完成 | Esc 返回改 key",
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
                Span::styled("输入: ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                Span::styled(
                    if self.input_buf.is_empty() { "…" } else { self.input_buf.as_str() },
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

        let p = Paragraph::new(lines).block(Block::default()).style(Style::default());
        f.render_widget(p, inner);
    }
}

/// 可用服务商列表:(名称, 端点, 默认模型)
fn provider_list() -> Vec<(String, String, String)> {
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

/// 按服务商名查找预置 key 环境变量的值(存在且已设置则读)
fn env_key_for(provider: &str) -> String {
    let cfg = Config::load().unwrap_or_default();
    let all = cfg.all_providers();
    if let Some(def) = all.get(provider) {
        if let Some(env) = &def.api_key_env {
            if let Ok(v) = std::env::var(env) {
                if !v.is_empty() {
                    return v;
                }
            }
        }
    }
    String::new()
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

    /// 轮数上限是独立一步(第 4 步):数字直接输入、0 = 不限、留空 = 默认、乱填报错重输
    #[test]
    fn max_turns_step_edits_and_verifies() {
        let mut w = SetupWizard::new();
        w.provider = "ollama".into();
        w.model = "qwen3:8b".into();

        // API Key 那步按 Enter → 进入轮数上限那步
        w.step = Step::ApiKey;
        match w.on_key(key(KeyCode::Enter)) {
            WizardAction::None => {}
            _ => panic!("Enter 应进入下一步"),
        }
        assert_eq!(w.step, Step::MaxTurns);
        assert_eq!(w.max_turns_label(), format!("默认({} 轮)", znaide_core::session::DEFAULT_MAX_TURNS));

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

        // 本步按 Enter → 验证(带上当前轮数上限)
        w.max_turns = Some(300);
        match w.on_key(key(KeyCode::Enter)) {
            WizardAction::Verify(r) => assert_eq!(r.model, "qwen3:8b"),
            _ => panic!("Enter 应触发验证"),
        }
        assert_eq!(w.step, Step::Verifying);

        // Esc 从验证回退到本步
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

    #[test]
    fn select_provider_fetches_models() {
        let mut w = SetupWizard::new();
        // 找到 deepseek(内置),Enter
        if let Some(i) = w.providers.iter().position(|(n, _, _)| n == "deepseek") {
            w.cursor = i;
        }
        match w.on_key(key(KeyCode::Enter)) {
            WizardAction::FetchModels { base_url, .. } => {
                assert!(base_url.contains("deepseek"));
            }
            _ => panic!("应触发模型查询"),
        }
        assert_eq!(w.step, Step::Querying);
        assert_eq!(w.provider, "deepseek");
    }

    #[test]
    fn model_injection_then_key_step() {
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
        assert_eq!(w.step, Step::ApiKey);
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
        assert_eq!(w.step, Step::ApiKey);
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
}
