//! 配置向导状态机(服务商 → 模型 → Key → 验证)。
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
        }
    }

    pub fn is_idle_step(&self) -> bool {
        matches!(self.step, Step::Provider | Step::ModelSelect | Step::ApiKey)
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
            }
            Err(e) => {
                self.models.clear();
                self.notice = format!("模型列表查询失败: {e}\n直接输入模型名(回车继续):");
                self.typing = true;
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
                    self.provider = name.clone();
                    self.api_key = env_key_for(&name);
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
                    // 验证
                    self.step = Step::Verifying;
                    self.progress = format!("正在验证 {} / {} …", self.provider, self.model);
                    WizardAction::Verify(self.draft())
                }
                KeyCode::Char('e') | KeyCode::Char('E') | KeyCode::Enter => {
                    // 直接编辑 key(覆盖)
                    self.typing = true;
                    self.input_buf = std::mem::take(&mut self.api_key);
                    WizardAction::None
                }
                _ => WizardAction::None,
            },
            Step::Verifying => {
                // 等待宿主;按 Esc 回 ApiKey
                if matches!(key.code, KeyCode::Esc) {
                    self.step = Step::ApiKey;
                }
                WizardAction::None
            }
        }
    }

    /// 处理输入框提交的内容(取决于当前阶段)
    fn confirm_typed(&mut self, v: &str) -> WizardAction {
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
        let steps = ["1 服务商", "2 模型", "3 API Key", "4 验证"];
        let current = match self.step {
            Step::Provider => 0,
            Step::Querying | Step::ModelSelect => 1,
            Step::ApiKey | Step::Verifying => 2,
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
                    "(未设置)".to_string()
                } else {
                    "••••••••".to_string()
                };
                lines.push(Line::from(vec![
                    Span::styled("API Key: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(key_disp, Style::default().fg(Color::White)),
                ]));
                lines.push(Line::from(Span::styled(
                    "e 输入 key(本地服务可留空)| s 验证并完成 | Esc 返回改模型",
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
