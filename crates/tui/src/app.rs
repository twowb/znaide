use crate::config_ui::{SetupWizard, WizardAction};
use crate::quick_cfg::{parse_qc_alias, QcItem, QuickCfgAction, QuickCfgPanel, QcStep};
use crate::sessions_ui::{SessionsUi, UiAction};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use ratatui::Terminal;
use std::io::stdout;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use znaide_core::config::Resolved;
use znaide_core::llm::build_llm_client;
use znaide_core::permissions::Mode;
use znaide_core::session::{
    list_history_sessions_detailed, remove_session, Session, SessionEvent, SharedInbox,
};

/// 配置向导异步结果回传
enum WizardReply {
    Models(Result<Vec<String>, String>),
    Verify(Result<(), String>),
}

/// 发送给 agent 的命令
enum AgentCmd {
    Prompt(String),
    SetMode(Mode),
    /// 从历史文件恢复会话
    LoadHistory(PathBuf),
    /// 运行时重配置(切换 provider/模型/端点/key,立即生效)
    Reconfigure(Resolved),
    /// 手动触发技能:/技能名 [参数](可能含入口脚本,由 agent 侧统一执行)
    RunSkill {
        name: String,
        args: String,
    },
    /// 清空会话上下文与历史文件(经确认后发送)
    ClearContext,
    /// 压缩上下文(旧消息 → 摘要,腾出上下文空间)
    Compact,
    /// 切换全局人格(/persona <名字>;空/无 = 关闭),写回配置持久生效
    SetPersona(String),
}

/// 共享取消槽:UI 侧随时可取消"当前回合"
type SharedCancel = std::sync::Arc<std::sync::Mutex<Option<CancellationToken>>>;

/// UI 输入事件:按键或粘贴文本
enum UiEvent {
    Key(crossterm::event::KeyEvent),
    Paste(String),
}

/// 消息区条目
enum MsgItem {
    User(String),
    Assistant(String),
    AssistantStream(String),
    Reasoning(String),
    Tool {
        name: String,
        /// 调用参数(pretty JSON):执行中与完成后都显示,知道 AI 在调什么
        args: String,
        ok: Option<bool>,
        output: String,
        /// 启动时刻:执行中的命令靠它动态计时
        started: Option<Instant>,
        /// 完成瞬间定格的总用时秒数(渲染用固定值,不再随帧跳动)
        done_secs: Option<u64>,
        /// AI 预估时长毫秒(模型传值;没传 = 没承诺时长,不显示预算段)
        idle_ms: Option<u64>,
        /// 静默预警档:0 无 / 1 黄(长时间无输出)/ 2 红(即将被静默超时终止)
        silent: u8,
        /// 执行中的实时输出累积(含 ANSI,有长度上限;完成时清空回落)
        live: String,
    },
    Notice(String),
    /// slash 命令输出(如 undo 列表)
    CommandOutput(String),
    /// 启动 Logo 横幅
    Logo,
}

/// 一次交互会话的结束统计(退出时由 CLI 打印)
pub struct ExitStats {
    pub session_id: String,
    pub history: Option<String>,
    pub uptime_secs: u64,
    pub user_msgs: u32,
    pub assistant_msgs: u32,
    pub tool_calls: u32,
    pub shell_calls: u32,
    pub file_writes: u32,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub undo_snapshots: usize,
}

/// 启动 Logo(用户选定的 ANSI 阴影风格字形,读作 ZNAIDE)
const LOGO_TEXT: &str = "
░█████████ ░███    ░██    ░███    ░██████░███████   ░██████████
      ░██  ░████   ░██   ░██░██     ░██  ░██   ░██  ░██
     ░██   ░██░██  ░██  ░██  ░██    ░██  ░██    ░██ ░██
   ░███    ░██ ░██ ░██ ░█████████   ░██  ░██    ░██ ░█████████
  ░██      ░██  ░██░██ ░██    ░██   ░██  ░██    ░██ ░██
 ░██       ░██   ░████ ░██    ░██   ░██  ░██   ░██  ░██
░█████████ ░██    ░███ ░██    ░██ ░██████░███████   ░██████████";

/// 拼出 Logo 的逐行文本
fn logo_lines() -> Vec<String> {
    LOGO_TEXT
        .lines()
        .filter(|l| !l.is_empty())
        .map(|s| s.to_string())
        .collect()
}

struct PermissionPrompt {
    title: String,
    body: String,
    /// Some(说明) = 高危命令或目标无法判定:红色渲染,且不提供"本会话都允许"
    warning: Option<String>,
    tx: oneshot::Sender<(bool, bool)>,
}

/// 破坏性操作确认(/clear 等),跟权限确认同款全屏弹窗
#[derive(Debug)]
enum ConfirmAction {
    ClearSession,
    /// 轮次预算用尽,问是否再放一批(引擎在等这个回答)
    ContinueRounds(oneshot::Sender<bool>),
}

struct ConfirmBox {
    title: String,
    body: String,
    action: ConfirmAction,
}

/// 光标感知的文本编辑工具(基于字节偏移,保持 char 边界)

/// 返回 cursor 前一个字符的字节起点(0 则返回 0)
fn cursor_prev(input: &str, cursor: usize) -> usize {
    let mut last = 0;
    for (i, _) in input.char_indices() {
        if i >= cursor {
            break;
        }
        last = i;
    }
    last
}

/// 返回 cursor 后一个字符的字节终点(到结尾则返回 len)
fn cursor_next(input: &str, cursor: usize) -> usize {
    let mut c = cursor.min(input.len());
    for (i, ch) in input.char_indices() {
        if i >= cursor {
            c = i + ch.len_utf8();
            return c;
        }
    }
    c
}

/// 把字符插入到光标处,返回新光标
fn insert_at(input: &mut String, cursor: usize, ch: char) -> usize {
    let at = cursor.min(input.len());
    input.insert(at, ch);
    at + ch.len_utf8()
}

/// 把字符串插入到光标处,返回新光标(移到插入内容之后)
fn insert_str_at(input: &mut String, cursor: usize, s: &str) -> usize {
    let at = cursor.min(input.len());
    input.insert_str(at, s);
    at + s.len()
}

/// 删除光标前一个字符,返回新光标
fn backspace_at(input: &mut String, cursor: usize) -> usize {
    if cursor == 0 {
        return 0;
    }
    let prev = cursor_prev(input, cursor);
    input.drain(prev..cursor);
    prev
}

/// 删除光标处一个字符,返回新光标
fn delete_at(input: &mut String, cursor: usize) -> usize {
    if cursor >= input.len() {
        return cursor;
    }
    let next = cursor_next(input, cursor);
    input.drain(cursor..next);
    cursor
}

/// 文本输入区按键处理结果
enum TextAction {
    None,
    Insert(char),
    Backspace,
    Newline,
    /// 回车发送(内容由宿主从 input 取)
    Submit,
}

/// 纯函数:处理"输入普通文本/回车"类按键(不含 Esc/方向键/全局命令)。
/// 便于单测换行语义。
fn handle_text_key(key: crossterm::event::KeyEvent) -> TextAction {
    use crossterm::event::KeyModifiers as KM;
    match key.code {
        KeyCode::Char(c) => {
            // Ctrl+J(0x0A)在 raw 模式下被 crossterm 解析为 Char('j')+CONTROL → 换行
            if c == 'j' && key.modifiers.contains(KM::CONTROL) {
                TextAction::Newline
            } else {
                TextAction::Insert(c)
            }
        }
        KeyCode::Enter => {
            // Shift+Enter / Alt+Enter 换行;普通 Enter 发送(内容由宿主取)
            if key.modifiers.contains(KM::SHIFT) || key.modifiers.contains(KM::ALT) {
                TextAction::Newline
            } else {
                TextAction::Submit
            }
        }
        KeyCode::Backspace => TextAction::Backspace,
        _ => TextAction::None,
    }
}

/// 启用鼠标捕获:Unix/Termux 手写 ANSI `1000+1002+1006`(1003 不用);
/// Windows 不吃 VT 序列,只能走 crossterm 的 console API。
#[cfg(unix)]
fn enable_mouse_capture() -> std::io::Result<()> {
    use std::io::Write as _;
    let mut out = std::io::stdout();
    out.write_all(b"\x1b[?1000h\x1b[?1002h\x1b[?1006h")?;
    out.flush()
}
#[cfg(windows)]
fn enable_mouse_capture() -> std::io::Result<()> {
    crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture)
}

/// 与 enable_mouse_capture 镜像的禁用(顺序反转)。
#[cfg(unix)]
fn disable_mouse_capture() -> std::io::Result<()> {
    use std::io::Write as _;
    let mut out = std::io::stdout();
    out.write_all(b"\x1b[?1006l\x1b[?1002l\x1b[?1000l")?;
    out.flush()
}
#[cfg(windows)]
fn disable_mouse_capture() -> std::io::Result<()> {
    crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture)
}

/// 生效的轮数上限:命令行 `--max-turns` 覆盖 > config 的 `max_turns`(0 = 不限)> 200。
/// 启动与 `/config` 重配都走它,让"命令行优先"在交互模式里同样成立。
fn effective_max_turns(cli_override: Option<usize>) -> usize {
    cli_override.unwrap_or_else(|| {
        znaide_core::config::Config::load()
            .map(|c| c.effective_max_turns())
            .unwrap_or(znaide_core::session::DEFAULT_MAX_TURNS)
    })
}

/// need03:按需直改落盘(只写目标字段,不调全量 `save()`)。
/// 成功返回附加提示(06 §2 的 CLI/ENV 覆盖诚实提示);失败返回原因(面板停留,不落盘)。
fn quick_cfg_save(
    panel: &QuickCfgPanel,
    item: QcItem,
    fallback_provider: &str,
    cli_max_turns: Option<usize>,
) -> anyhow::Result<String> {
    let mut cfg = znaide_core::config::Config::load().unwrap_or_default();
    let provider = if panel.provider.is_empty() {
        fallback_provider
    } else {
        &panel.provider
    };
    if provider.is_empty() {
        anyhow::bail!("先选服务商(菜单第 1 项),再改其他项。");
    }
    let mut extra = String::new();
    match item {
        QcItem::Provider => {
            cfg.switch_provider(provider)?;
            extra = "可在菜单继续微调该家字段,建议跑一次验证。".into();
        }
        QcItem::Model => {
            if panel.model.trim().is_empty() {
                anyhow::bail!("模型名为空,先选一个或按 m 手输。");
            }
            cfg.save_model(provider, panel.model.trim())?;
        }
        QcItem::Protocol => {
            cfg.save_protocol(provider, panel.protocol)?;
        }
        QcItem::ApiKey => {
            // env 家未手输 → None=不动(不把环境变量值落盘,04 §4 守卫)
            let key_arg: Option<&str> = if panel.key_from_env {
                None
            } else {
                Some(&panel.api_key)
            };
            cfg.save_api_key(provider, key_arg)?;
            if panel.key_from_env {
                extra = "(仍走环境变量,未落盘)".into();
            }
        }
        QcItem::BaseUrl => {
            cfg.save_base_url(provider, &panel.base_url)?;
        }
        QcItem::ContextWindow => {
            cfg.save_context_window(provider, panel.context_window)?;
        }
        QcItem::MaxTurns => {
            cfg.save_max_turns(panel.max_turns)?;
            // CLI --max-turns 仍优先:文件存了但本次运行没生效,必须明说(06 §2)
            if cli_max_turns.is_some() {
                extra = "(当前轮数上限被命令行参数覆盖,配置文件已存但本次运行仍以命令行值为准)".into();
            }
        }
        QcItem::Retry => {
            cfg.save_retry(panel.retry)?;
            // ENV/CLI 覆盖重试时同样诚实提示(TUI 侧能检测到 ENV,CLI 值未传入则不提)
            let env_hit = ["ZNAIDE_NO_RETRY", "ZNAIDE_RETRY"].iter().any(|n| {
                std::env::var(n)
                    .map(|v| !v.trim().is_empty())
                    .unwrap_or(false)
            });
            if env_hit {
                extra = "(当前重试被环境变量覆盖,配置文件已存但本次运行仍以环境变量为准)".into();
            }
        }
        QcItem::SessionHeader => {
            cfg.save_session_header(provider, panel.session_header)?;
        }
        QcItem::Proxy => {
            cfg.save_proxy(panel.proxy.clone())?;
            // Off/环境覆盖时的诚实提示（照抄 MaxTurns/Retry 的 extra 口径）
            if std::env::var("ZNAIDE_PROXY")
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false)
            {
                extra =
                    "(当前代理被 ZNAIDE_PROXY 环境变量覆盖，文件已存但本次运行仍以环境变量为准)"
                        .into();
            }
        }
    }
    Ok(extra)
}

/// need03:保存后生效四件套(06 §1,与 `/config` 全向导同口径):
/// 读最新文件 resolve → Reconfigure → 刷 round_limit/ctx_window/current_resolved。
/// `verified` 决定 Notice 是"验证通过"还是"未验证"口径。
#[allow(clippy::too_many_arguments)]
fn quick_cfg_apply_saved(
    cmd_tx: &mpsc::UnboundedSender<AgentCmd>,
    current_resolved: &mut Resolved,
    ctx_window: &mut Option<usize>,
    round_limit: &mut usize,
    configured_ok: &mut bool,
    items: &mut Vec<MsgItem>,
    cli_max_turns: Option<usize>,
    headline: &str,
) {
    let Ok(cfg) = znaide_core::config::Config::load() else {
        items.push(MsgItem::Notice("⚠ 配置文件刚写入却读不回来,请检查磁盘权限。".into()));
        return;
    };
    let Ok(draft) = cfg.resolve(None, None, None, None, None, None) else {
        items.push(MsgItem::Notice("⚠ 配置已存但解析失败(模型/端点缺失?),请检查。".into()));
        return;
    };
    // 协议变了 → 沿用全向导 proto_note 文案(06 §3)
    let proto_note = if current_resolved.protocol != draft.protocol {
        format!(
            "协议已切换为 {}，历史继续有效；若模型表现异常可 /clear 开新上下文。",
            draft.protocol.as_str()
        )
    } else {
        String::new()
    };
    *round_limit = effective_max_turns(cli_max_turns);
    let _ = cmd_tx.send(AgentCmd::Reconfigure(draft.clone()));
    *current_resolved = draft.clone();
    *ctx_window = current_resolved.effective_context_window();
    *configured_ok = true;
    items.push(MsgItem::Notice(format!("✔ {headline}{proto_note}")));
}

/// 交互主循环。resume = `--resume` 指定的历史会话文件;
/// persona = 启动时注入的全局人格(空 = 不注入)。
/// max_turns = 命令行 `--max-turns` 覆盖(None = 用 config/默认)。
/// 退出(/quit、/exit、Ctrl+C)时返回本次统计,由 CLI 打印。
pub async fn run(
    resolved: &Resolved,
    mode: Mode,
    cwd: &Path,
    first_run: bool,
    resume: Option<PathBuf>,
    persona: String,
    max_turns: Option<usize>,
) -> anyhow::Result<ExitStats> {
    enable_raw_mode()?;
    let mut stdout = stdout();
    // 启用 bracketed paste(多行粘贴整段到达)
    let _ = execute!(
        stdout,
        EnterAlternateScreen,
        crossterm::event::EnableBracketedPaste
    );
    // 鼠标:1000 点击 + 1002 按住拖动(滚动条)+ 1006 SGR 坐标,不开 1003。
    // 不开 1003 是怕纯移动也上报,拖选/拖动时海量 SGR 事件跟 shell 子进程抢 stdin,
    // 把字节流撕成残片塞进输入框(踩过)。鼠标被接管后原生拖选失效,
    // 复制要按住 Shift 再拖(界面有提示)。
    let _ = enable_mouse_capture();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let llm = build_llm_client(resolved)?;
    // 协议是 Copy:spawn 前取出,免得 async move 把 `resolved` 引用整个抓进去
    let session_protocol = resolved.protocol;
    // 会话头开关同样 spawn 前取出(跟 provider 走的配置快照)
    let session_header_enabled = resolved.session_header_enabled;
    // 弱网重试同样 spawn 前取出(RetryConfig 是 Copy)
    let session_retry = resolved.retry;
    // 生效代理同样 spawn 前取出(url 字符串；Session/更新检查/工具链路共用同一份快照)
    let session_proxy_url: Option<String> = resolved.proxy.url().map(|s| s.to_string());
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<SessionEvent>();
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<AgentCmd>();
    // 更新进度/结果通知(update task → UI)
    let (update_tx, mut update_rx) = mpsc::unbounded_channel::<String>();
    // 共享取消槽:每个回合的新 token 都放这里,UI 的 Esc 直接 cancel 它
    let shared_cancel: SharedCancel = std::sync::Arc::default();
    // 运行中插话的投递箱:UI 主循环入队,会话引擎在轮边界取走(两边看同一个队列)
    let inbox: SharedInbox = std::sync::Arc::default();

    let cwd_buf: PathBuf = cwd.to_path_buf();
    let shared_cancel_agent = shared_cancel.clone();
    let inbox_agent = inbox.clone();
    // --resume:直接"开在"被恢复的会话上(沿用其 ID/历史文件,后续对话续写回同一文件)
    let start_session_id: Option<String> = resume
        .as_ref()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().to_string()));

    // 当前人格(状态栏显示;启动取配置,切换经 PersonaChanged 事件更新)。
    // 必须在 agent task move persona 之前 clone
    let mut persona_disp = persona.clone();

    let agent_task = tokio::spawn(async move {
        let ev_broadcast = ev_tx.clone();
        // MCP 启动(配置存在时)
        let mcp_cfg_path = znaide_core::config::data_dir().join("mcp.json");
        let mcp = if mcp_cfg_path.exists() {
            let m = znaide_core::mcp::McpManager::start().await;
            for (name, err) in &m.errors {
                let _ = ev_broadcast.send(SessionEvent::Notice(format!("⚠ MCP {name}: {err}")));
            }
            if !m.client_names().is_empty() {
                let names = m.client_names().join(", ");
                let _ = ev_broadcast.send(SessionEvent::Notice(format!("🔌 MCP servers: {names}")));
            }
            Some(m)
        } else {
            None
        };
        let cancel = CancellationToken::new();
        let mut session = match Session::new(
            llm,
            session_protocol,
            cwd_buf,
            mode,
            Some(ev_tx.clone()),
            cancel.clone(),
            true,
            start_session_id.clone(),
            mcp,
            &persona,
            session_header_enabled,
        ) {
            Ok(s) => s,
            Err(_) => return,
        };
        // 轮数上限:--max-turns > config 的 max_turns(0 = 不限)> 默认。
        // 以前这里只读 config,`znaide --max-turns N` 在交互模式下被静默忽略。
        session.set_max_turns(effective_max_turns(max_turns));
        // 弱网重试:resolved.retry 已含 CLI/ENV/config 优先级结果；/config 重配经
        // Reconfigure → session.reconfigure() 即时更新。
        session.set_retry(session_retry);
        // 运行中插话:引擎在每个轮边界从这个投递箱取一条插进对话(见 InputQueue)
        session.set_inbox(inbox_agent);
        if !persona.is_empty() {
            let _ = ev_tx.send(SessionEvent::Notice(format!(
                "人格:{persona}(全局生效,输入 /persona 可切换或关闭)"
            )));
        }
        loop {
            tokio::select! {
                cmd = cmd_rx.recv() => match cmd {
                    Some(AgentCmd::Prompt(p)) => {
                        // 每个新回合:换新 token 并放入共享槽(旧 token 一经取消即失效,不可复用)
                        let round_cancel = CancellationToken::new();
                        *shared_cancel_agent.lock().unwrap() = Some(round_cancel.clone());
                        session.reset_cancel(round_cancel);
                        let _ = session.run_turn(&p).await;
                        // 回合结束,清空槽,避免误取消下一回合
                        *shared_cancel_agent.lock().unwrap() = None;
                    }
                    Some(AgentCmd::SetMode(m)) => {
                        session.set_mode(m);
                    }
                    Some(AgentCmd::RunSkill { name, args }) => {
                        let round_cancel = CancellationToken::new();
                        *shared_cancel_agent.lock().unwrap() = Some(round_cancel.clone());
                        session.reset_cancel(round_cancel);
                        let _ = session.run_skill_turn(&name, &args).await;
                        *shared_cancel_agent.lock().unwrap() = None;
                    }
                    Some(AgentCmd::Reconfigure(r)) => {
                        session.reconfigure(&r);
                        // 轮数上限也跟着重读(/config 改了 max_turns 立即生效;
                        // 命令行给了 --max-turns 的话仍然它优先)。
                        // 状态栏的 round_limit 由宿主在保存配置处同步(这里在 agent 任务里,拿不到 UI 变量)
                        session.set_max_turns(effective_max_turns(max_turns));
                        // 弱网重试同样即时跟上(/cfg 改 retry 立即生效;06 §1)。
                        // TUI 侧无 CLI retry 值,用文件值(与全向导 draft 同口径)。
                        session.set_retry(r.retry);
                        session.notify(format!(
                            "✔ 配置已切换: {} / {}",
                            r.provider_name, r.model
                        ));
                    }
                    Some(AgentCmd::LoadHistory(path)) => {
                        match session.load_history(&path) {
                            Ok(n) => {
                                session.notify(format!(
                                    "已恢复历史会话({} 条消息),可继续对话。历史文件:{}\n⚠ 恢复后最好核对一下上下文,模型未必理解此前的中间状态。",
                                    n, path.display()
                                ));
                            }
                            Err(e) => {
                                session.notify(format!("恢复历史失败: {e}"));
                            }
                        }
                    }
                    Some(AgentCmd::ClearContext) => {
                        // 清空上下文与历史文件;完成后经 ContextCleared 事件通知 UI
                        session.clear_context();
                    }
                    Some(AgentCmd::Compact) => {
                        // 进行中状态由 CompactionStarted / ContextCompacted / TurnFinished 事件驱动
                        match session.compact_context().await {
                            Ok(0) => session.notify("上下文还短,无需压缩。"),
                            Ok(_n) => {
                                // 完成提示经 ContextCompacted 事件展示(含摘要),无需重复通知
                            }
                            Err(e) => session.notify(format!("⚠ 上下文压缩失败: {e}")),
                        }
                    }
                    Some(AgentCmd::SetPersona(name)) => {
                        if let Err(e) = session.set_persona(&name) {
                            session.notify(format!("⚠ {e}"));
                        }
                    }
                    None => break,
                },
                _ = cancel.cancelled() => {}
            }
        }
    });

    let mut items: Vec<MsgItem> = vec![MsgItem::Logo];
    items.push(MsgItem::Notice(
        "znaide 就绪。Enter 发送(工作中 Enter 排队,下一个轮边界发出),\
         Shift+Enter(或 Alt+Enter / Ctrl+J)换行;\
         复制文字要按住 Shift 拖动选;退出用 /quit 或 /exit;/help 看全部命令。"
            .into(),
    ));
    // --resume:启动即恢复目标历史会话(HistoryLoaded 事件会把界面切换为恢复内容)
    if let Some(rp) = &resume {
        items.push(MsgItem::Notice(format!(
            "▶ 正在恢复会话 {}…",
            rp.file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default()
        )));
        let _ = cmd_tx.send(AgentCmd::LoadHistory(rp.clone()));
    }
    let mut input = String::new();
    // 输入光标(字节偏移,始终落在 char 边界)
    let mut input_cursor = 0usize;
    let mut busy = false;
    // 忙碌场景(回合执行 / 压缩),决定忙行动画与文案
    let mut busy_kind = BusyKind::Work;
    // 工作中输入队列:回合执行时 Enter 入队,引擎在轮边界插话(见 InputQueue)
    let mut queue = InputQueue::new(inbox);
    // /quit、/exit 请求退出(置位后主循环退出)
    let mut quit_requested = false;
    // 上下文 >=90% 的提示是否已显示(回落到 <90% 后重新武装)
    let mut ctx_warn_shown = false;
    // token 统计:会话输入/输出真实累计 + 当前回复实时估算
    let mut token_stats = TokenStats::default();
    let mut current_mode = mode;
    let mut permission: Option<PermissionPrompt> = None;
    // 破坏性操作确认(/clear 等)
    let mut confirm: Option<ConfirmBox> = None;
    // 输入补全(slash 命令 / @ 引用):菜单 + ghost
    let mut completion: Option<Completion> = None;
    // 滚动:从底部算起的偏移行数(0 = 跟随最新)。渲染层按总行数与可视高度夹取。
    let mut scroll_back = 0usize;
    // 消息区内容版本:每次绘制时若处于底部跟随则同步到底
    let mut at_bottom = true;
    // 上次渲染的消息区总行数:上翻看历史(不在底部)时,实时输出增长会把底
    // 部推远——把新增行数补偿进偏移,保证当前视野不被新行滑走
    let mut prev_total = 0usize;
    // 消息区几何与滚动范围(每帧绘制时刷新,供鼠标事件换算)
    let mut msg_area = Rect::default();
    let mut scroll_range = 0usize;
    // 是否正按住滚动条拖动
    let mut sb_dragging = false;
    // 当前生效配置(表单回填与状态栏展示用)
    let mut current_resolved = resolved.clone();
    // 生效上下文窗口:只随 current_resolved 变,别每帧重算
    // (effective_context_window 内部要先 to_lowercase 再跑一长串 contains)
    let mut ctx_window = current_resolved.effective_context_window();
    // 轮次:本轮已用轮数 / 上限(0 = 不限),状态栏展示用
    let mut round_used: usize = 0;
    let mut round_limit: usize = effective_max_turns(max_turns);
    // 当前会话 id(agent 经 SessionInfo 事件回传,状态栏展示用)
    let mut session_id = String::new();
    // 配置向导(None = 对话模式)
    let mut config_wizard: Option<SetupWizard> = None;
    // 按需直改面板(/cfg;None = 对话模式;与 config_wizard 互斥,同一时刻最多一个)
    let mut quick_cfg: Option<QuickCfgPanel> = None;
    // 会话管理窗口(/resume 无参打开;None = 对话模式)
    let mut sessions_ui: Option<SessionsUi> = None;
    // 配置向导异步回传通道
    let (wiz_tx, mut wiz_rx) = mpsc::unbounded_channel::<WizardReply>();
    // 是否已有可用配置(未配置成功时禁止使用)。与首启判定同源:first_run 就是
    // cli 那边 `!config_provided()`(配置文件 / 环境变量 / 命令行覆盖)的结果取反。
    // 以前这里单独查 `config_exists()`(只看文件),于是"环境变量或命令行把 provider、
    // model、key 都配齐了 → 不弹向导 → 却仍被判没配置、不能对话";同一件事在两个
    // 宿主各写一份就一定会漂,现在只剩一个来源。
    let mut configured_ok = !first_run;
    if first_run {
        items.push(MsgItem::Notice(
            "🎉 首次运行:先完成一次配置。选 AI 服务商,向导自动查询可用模型。".into(),
        ));
        items.push(MsgItem::Notice(
            "配置完成并验证通过前对话不可用。需要时用 /config 重开向导。".into(),
        ));
        let mut w = SetupWizard::new();
        w.prefill();
        config_wizard = Some(w);
    }

    // 动画时钟(与 ~120ms 帧对齐)
    let app_start = std::time::Instant::now();

    // 启动自动更新检查(默认开):界面起来 2s 后静默探测一次,发现新版才提示;
    // 网络失败静默不打扰,随时可 /update 手动检查。探测按 Gitee → GitHub 顺序,
    // 任一源连通即用(Gitee 优先,国内直连无需代理)。
    {
        let update_tx = update_tx.clone();
        let proxy_url = session_proxy_url.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let Ok(client) = znaide_core::update::http_client_with_proxy(proxy_url.as_deref())
            else {
                return;
            };
            let Ok((src, latest)) = znaide_core::update::probe_latest(&client).await else {
                return;
            };
            let cur = znaide_core::update::current_version();
            if znaide_core::update::version_gt(&latest, &cur) {
                let _ = update_tx.send(format!(
                    "发现新版本 v{latest}(来源 {})\n当前 v{cur}:输入 /update 立即更新,或忽略继续使用。",
                    src.label()
                ));
            }
        });
    }
    loop {
        {
            terminal.draw(|f| {
                // 动画帧号:同屏同相;各动画点按需取子相位
                let anim_frame = app_start.elapsed().as_millis() as usize / 120;
                // 布局:头部 / 消息区 / 输入区(高度随内容动态,上限 MAX_INPUT_ROWS)/ 状态栏。
                // 输入区默认 1 行;换行/折行时逐行增高并即时回缩;内容超过上限后
                // 输入框内部滚动跟随光标(render_input 的窗口逻辑)。消息区最少 3 行。
                let label = if permission.is_some() || confirm.is_some() {
                    " [等待确认]"
                } else if busy {
                    " [Enter 排队]"
                } else {
                    ""
                };
                let (prefix_first, _) = input_prefix(label);
                // 与 render_input 相同的前缀扣除与最小 body 宽,保证折行结果一致
                let input_body_w = (f.area().width.saturating_sub(2) as usize)
                    .saturating_sub(prefix_first.chars().count())
                    .max(4);
                let input_rows: u16 =
                    input_phys_lines(&input, input_body_w).min(MAX_INPUT_ROWS) as u16;
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(1),
                        Constraint::Min(3),
                        Constraint::Length(input_rows + 2),
                        Constraint::Length(1),
                    ])
                    .split(f.area());
                // 固定头部
                let header = Line::from(vec![
                    Span::styled(
                        " znaide ",
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(
                            " v{} 无所不能的终端 AI 助手 | {}",
                            env!("CARGO_PKG_VERSION"),
                            cwd.display()
                        ),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]);
                f.render_widget(Paragraph::new(header), chunks[0]);
                // 配置向导模式:覆盖消息+输入+状态区
                if let Some(wizard) = &config_wizard {
                    // 限制向导区不超出屏幕:从消息区起点到状态栏上沿
                    let avail_top = chunks[1].y;
                    let avail_bottom = chunks[3].y; // 状态栏 y
                    let inner = Rect {
                        x: chunks[1].x,
                        y: avail_top,
                        width: chunks[1].width,
                        height: avail_bottom.saturating_sub(avail_top).max(1),
                    };
                    wizard.render(f, inner);
                    return;
                }
                // 按需直改面板(/cfg):同样覆盖消息+输入+状态区
                if let Some(qc) = &quick_cfg {
                    let avail_top = chunks[1].y;
                    let avail_bottom = chunks[3].y; // 状态栏 y
                    let inner = Rect {
                        x: chunks[1].x,
                        y: avail_top,
                        width: chunks[1].width,
                        height: avail_bottom.saturating_sub(avail_top).max(1),
                    };
                    qc.render(f, inner);
                    return;
                }
                // 会话管理窗口:同样覆盖消息+输入+状态区
                if let Some(su) = &sessions_ui {
                    let avail_top = chunks[1].y;
                    let avail_bottom = chunks[3].y;
                    let inner = Rect {
                        x: chunks[1].x,
                        y: avail_top,
                        width: chunks[1].width,
                        height: avail_bottom.saturating_sub(avail_top).max(1),
                    };
                    su.render(f, inner);
                    return;
                }

                // ---- 消息区:先渲染全部逻辑行,再按可视高度与滚动偏移切片 ----
                let content_w = chunks[1].width.saturating_sub(2).max(20);
                let all_lines = build_content_lines(&items, busy, busy_kind, content_w, anim_frame);
                let total = all_lines.len();
                let view_h = chunks[1].height.saturating_sub(2) as usize;

                // 内容不足一屏或处于底部跟随 → 偏移归零;上翻看历史时,实时
                // 输出的新行会把底部推远——偏移同步增加,视野保持不动
                if at_bottom || total <= view_h {
                    scroll_back = 0;
                } else {
                    scroll_back = scroll_back.saturating_add(total.saturating_sub(prev_total));
                    scroll_back = scroll_back.min(total - view_h);
                }
                prev_total = total;

                // 可视窗口:从底部算起 scroll_back 行之上,向上取 view_h 行
                // scroll_back=0 → 最后 view_h 行(最新)
                let start = total.saturating_sub(view_h + scroll_back);
                let end = (start + view_h).min(total);
                let window: Vec<Line<'static>> = all_lines[start..end].to_vec();

                // 输出框(消息区)边框 = 当前权限模式色(与输入框一致,常显不随 busy 变灰)
                let msg_border_color = mode_color(current_mode);
                let p = Paragraph::new(window).block(
                    Block::default()
                        .borders(ratatui::widgets::Borders::ALL)
                        .border_style(Style::default().fg(msg_border_color)),
                );
                f.render_widget(p, chunks[1]);
                // 记录几何供鼠标事件换算(滚动条列、可视行数)
                msg_area = chunks[1];
                scroll_range = total.saturating_sub(view_h);
                // 消息区滚动条:贴右边框画一段拇指,位置随 scroll_back 移动
                draw_msg_scrollbar(f, chunks[1], total, view_h, scroll_back);
                // 输入框边框颜色 = 当前权限模式(询问绿 / 编辑放行天蓝 / 全自动紫 / 超级红)。
                // 工作中不再整框变灰:忙时照样能输入(Enter 入队),灰掉会让人以为不能用。
                let border_color = mode_color(current_mode);
                // 多行输入渲染(带光标指示):把光标所在位置渲染成反白块。
                // 可视行 = 动态 input_rows(内容不足即包住内容,超过上限则内部滚动)。
                // ghost 预览仅在光标位于整段末尾时给出
                let input_h = input_rows as usize;
                let ghost = if input_cursor == input.len() {
                    completion.as_ref().and_then(ghost_tail)
                } else {
                    None
                };
                let input_para = render_input(
                    &input,
                    input_cursor,
                    label,
                    Style::default(),
                    input_h,
                    chunks[2].width.saturating_sub(2),
                    ghost.as_deref(),
                )
                .block(
                    Block::default()
                        .borders(ratatui::widgets::Borders::ALL)
                        .border_style(Style::default().fg(border_color)),
                );
                f.render_widget(input_para, chunks[2]);
                // 补全菜单:浮在输入框上方
                if let Some(cm) = &completion {
                    if !cm.items.is_empty() {
                        draw_completion_menu(f, chunks[2], cm);
                    }
                }
                // 状态栏:显示模式/模型/token 统计/滚动状态
                let scroll_hint = if scroll_back > 0 {
                    format!(" ↕ 上翻{}行(End回底)", scroll_back)
                } else {
                    String::new()
                };
                let sess = if session_id.is_empty() {
                    String::new()
                } else {
                    format!(" · 会话 {}", session_id)
                };
                // token 统计:输入/输出/会话总计。
                // **这是计费口径的累计**:每次模型调用都会把完整历史重发一遍,服务端按
                // 每份 prompt 各计一次,所以 Σ 会随"轮数 × 上下文长度"涨,不等于上下文占用
                // (占用看 ctx 条)。/clear、/resume 换会话时归零,/compact 不归零(同一会话,
                // 已经花掉的账不该消失)。
                // 进行中回复的真实 usage 尚未到账,输出/总计以 ≈ 附上实时估算;
                // 该轮结束(Usage 事件)即切换为真实值。
                let token_info = {
                    let sum_real = token_stats.input + token_stats.output;
                    let live = busy && token_stats.round_est > 0;
                    let out_show =
                        token_stats.output + if live { token_stats.round_est } else { 0 };
                    let sum_show = sum_real + if live { token_stats.round_est } else { 0 };
                    if sum_show > 0 {
                        let mark = if live { "≈" } else { "" };
                        format!(
                            " · 累计 in {} · out {}{} · Σ {}{}",
                            fmt_tokens(token_stats.input),
                            mark,
                            fmt_tokens(out_show),
                            mark,
                            fmt_tokens(sum_show),
                        )
                    } else {
                        String::new()
                    }
                };
                // 工作中/压缩中:状态栏按场景播放动画(执行=流动光条,压缩=收纳推进);
                // 排了队就附上条数,免得以为排的消息丢了
                let queue_seg = if queue.is_empty() {
                    String::new()
                } else {
                    format!(" · 队列 {}", queue.len())
                };
                let state_text = if busy {
                    match busy_kind {
                        BusyKind::Work => format!("工作中… {}{queue_seg}", wave_row(anim_frame, 8)),
                        BusyKind::Compacting => {
                            format!("压缩中… {}{queue_seg}", shrink_char(anim_frame / 2))
                        }
                    }
                } else if !queue_seg.is_empty() {
                    // 空闲却还有排队(被弹窗挡着放行):如实显示,别让人干等
                    format!("就绪{queue_seg}")
                } else {
                    "就绪".to_string()
                };
                // 状态栏人格段(未注入人格不显示)
                let persona_seg = if persona_disp.is_empty() {
                    String::new()
                } else {
                    format!(" | {}", persona_disp)
                };
                // 轮次段:本轮已用 / 上限(0 = 不限)
                let round_info = format!(
                    " · 轮 {}/{}",
                    round_used,
                    if round_limit == 0 {
                        "∞".to_string()
                    } else {
                        round_limit.to_string()
                    }
                );
                let status = format!(
                    " {}{} | {} | {}{}{}{}{}",
                    current_mode.label(),
                    persona_seg,
                    current_resolved.model,
                    state_text,
                    round_info,
                    token_info,
                    scroll_hint,
                    sess
                );
                let mut status_spans =
                    vec![Span::styled(status, Style::default().fg(Color::DarkGray))];
                // 上下文占用条(真实 prompt / 模型窗口),高占用变色
                if let Some((badge, badge_color)) =
                    ctx_usage_badge(token_stats.last_prompt, ctx_window)
                {
                    status_spans.push(Span::styled(
                        format!(" {badge}"),
                        Style::default()
                            .fg(badge_color)
                            .add_modifier(Modifier::BOLD),
                    ));
                }
                f.render_widget(Paragraph::new(Line::from(status_spans)), chunks[3]);
                if let Some(pp) = &permission {
                    draw_permission(f, f.area(), pp);
                }
                if let Some(cf) = &confirm {
                    draw_confirm_popup(
                        f,
                        f.area(),
                        &cf.title,
                        &cf.body,
                        "y 确认 | n / Esc 取消",
                        false,
                    );
                }
            })?;
        }

        let mut mouse_ev: Option<crossterm::event::MouseEvent> = None;
        let key_ev = if event::poll(Duration::from_millis(120))? {
            match event::read()? {
                // Windows 上 crossterm 对一次按键连发 Press + Release 两个事件,
                // Unix 只发 Press。非 Press(Release/Repeat)必须丢弃,
                // 否则同一按键会被当成按了两次。
                Event::Key(k) if k.kind == KeyEventKind::Press => Some(UiEvent::Key(k)),
                Event::Key(_) => None,
                Event::Paste(text) => Some(UiEvent::Paste(text)),
                Event::Mouse(m) => {
                    mouse_ev = Some(m);
                    None
                }
                _ => None,
            }
        } else {
            None
        };

        // 鼠标:消息区内滚轮滚动;按住右侧滚动条拖动/点击跳转
        if let Some(m) = mouse_ev {
            handle_mouse_scroll(
                m,
                msg_area,
                scroll_range,
                &mut scroll_back,
                &mut at_bottom,
                &mut sb_dragging,
            );
        }

        // 粘贴事件:整段进入输入框(不经逐键路径,避免误触发发送)
        if let Some(UiEvent::Paste(text)) = &key_ev {
            // 兼容 \r\n 与 \r;再剥离终端控制/转义残留(鼠标残片防线)
            let norm = text.replace("\r\n", "\n").replace('\r', "\n");
            let clean = sanitize_typed_text(&norm);
            if config_wizard.is_none()
                && quick_cfg.is_none()
                && permission.is_none()
                && confirm.is_none()
            {
                input_cursor = insert_str_at(&mut input, input_cursor, &clean);
            } else if permission.is_none() && confirm.is_none() {
                // 配置向导/按需面板的文本输入态(API Key / 端点 / 模型名 / 轮数)允许粘贴:
                // 只填进输入缓冲,**不当作回车**(不提交、不推进步骤)。
                // 权限确认与破坏性确认弹窗是 y/n 决策,粘贴仍忽略(防多行粘贴误触发)。
                if let Some(w) = config_wizard.as_mut() {
                    let _ = w.paste_text(&clean);
                }
                if let Some(q) = quick_cfg.as_mut() {
                    let _ = q.paste_text(&clean);
                }
            }
        }

        if let Some(UiEvent::Key(k)) = key_ev {
            if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
                break;
            }
            if let Some(pp) = permission.take() {
                let danger = pp.warning.is_some();
                match k.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                        let _ = pp.tx.send((true, false));
                    }
                    // 高危/目标无法判定的命令不提供"本会话都允许"
                    KeyCode::Char('a') | KeyCode::Char('A') if !danger => {
                        let _ = pp.tx.send((true, true));
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                        let _ = pp.tx.send((false, false));
                    }
                    _ => {
                        permission = Some(pp);
                    }
                }
                continue;
            }
            // 确认框:权限以外的破坏性操作(/clear)与轮次续跑询问共用
            if let Some(cf) = confirm.take() {
                match k.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => match cf.action {
                        ConfirmAction::ClearSession => {
                            // 界面立即清;上下文与历史文件的清空由 agent 完成(ContextCleared 事件同步)
                            items.clear();
                            let _ = cmd_tx.send(AgentCmd::ClearContext);
                        }
                        ConfirmAction::ContinueRounds(tx) => {
                            let _ = tx.send(true);
                            items.push(MsgItem::Notice("▶ 继续执行,再放一批轮次。".into()));
                        }
                    },
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => match cf.action {
                        ConfirmAction::ContinueRounds(tx) => {
                            let _ = tx.send(false);
                            items.push(MsgItem::Notice(
                                "已停在当前进度;要继续直接再发一条消息。".into(),
                            ));
                        }
                        ConfirmAction::ClearSession => {
                            items.push(MsgItem::Notice("已取消清空。".into()));
                        }
                    },
                    _ => {
                        confirm = Some(cf);
                    }
                }
                continue;
            }
            // 补全菜单 ↑↓/Tab/Enter/Esc 都在这里处理。↑↓ 只动选中,菜单必须放回
            // completion,否则每帧 refresh 会当"新建"重建、选中归零;Esc 则放回空项
            // 占位,免得 refresh 按原前缀立刻把菜单又复活。
            if let Some(mut cm) = completion.take() {
                if cm.items.is_empty() {
                    completion = Some(cm);
                } else {
                    match k.code {
                        KeyCode::Up => {
                            cm.selected = cm.selected.saturating_sub(1);
                            completion = Some(cm);
                            continue;
                        }
                        KeyCode::Down => {
                            cm.selected = (cm.selected + 1).min(cm.items.len() - 1);
                            completion = Some(cm);
                            continue;
                        }
                        KeyCode::Tab if !k.modifiers.contains(KeyModifiers::SHIFT) => {
                            accept_completion(&mut input, &mut input_cursor, &cm);
                            continue; // 输入已变化,菜单交由下一帧 refresh 重建/关闭
                        }
                        KeyCode::Enter
                            if !k.modifiers.contains(KeyModifiers::SHIFT)
                                && !k.modifiers.contains(KeyModifiers::ALT) =>
                        {
                            accept_completion(&mut input, &mut input_cursor, &cm);
                            continue;
                        }
                        KeyCode::Esc => {
                            // 手动关闭:占位空菜单,前缀没变 refresh 不会重建;
                            // 输入一变,typed 变了菜单自然重新打开
                            completion = Some(Completion {
                                kind: cm.kind,
                                typed: cm.typed.clone(),
                                word_start: cm.word_start,
                                items: Vec::new(),
                                selected: 0,
                            });
                            continue;
                        }
                        _ => completion = Some(cm),
                    }
                }
            }
            // 配置向导模式:按键交给向导,动作由宿主异步执行
            if let Some(mut wizard) = config_wizard.take() {
                let action = wizard.on_key(k);
                match action {
                    WizardAction::None => {
                        config_wizard = Some(wizard);
                    }
                    WizardAction::Exit => {
                        items.push(MsgItem::Notice(if configured_ok {
                            "已关闭配置向导,用 /config 可再打开。".into()
                        } else {
                            "⚠ 配置还没完成,对话仍不可用。用 /config 继续配置。".into()
                        }));
                    }
                    WizardAction::FetchModels {
                        base_url,
                        api_key,
                        session_header,
                        proxy,
                    } => {
                        // 保留向导(Querying 状态),后台查询模型
                        config_wizard = Some(wizard);
                        let tx = wiz_tx.clone();
                        tokio::spawn(async move {
                            let r = znaide_core::llm::openai::probe_models(
                                &base_url,
                                api_key.as_deref(),
                                session_header.as_deref(),
                                &proxy,
                            )
                            .await;
                            let _ = tx.send(WizardReply::Models(r.map_err(|e| format!("{e:#}"))));
                        });
                    }
                    WizardAction::Verify(draft) => {
                        config_wizard = Some(wizard);
                        let tx = wiz_tx.clone();
                        let probe = draft.clone();
                        tokio::spawn(async move {
                            let r = znaide_core::llm::openai::probe_chat(&probe).await;
                            let _ = tx.send(WizardReply::Verify(r.map_err(|e| format!("{e:#}"))));
                        });
                    }
                }
                continue;
            }
            // 按需直改面板(/cfg):按键交面板,保存/验证由宿主执行(与全向导对称)
            if let Some(mut qc) = quick_cfg.take() {
                let action = qc.on_key(k);
                match action {
                    QuickCfgAction::None => {
                        quick_cfg = Some(qc);
                    }
                    QuickCfgAction::Exit => {
                        items.push(MsgItem::Notice("已关闭按需配置(随时 /cfg 再开)。".into()));
                    }
                    QuickCfgAction::FetchModels {
                        base_url,
                        api_key,
                        session_header,
                        proxy,
                    } => {
                        quick_cfg = Some(qc);
                        let tx = wiz_tx.clone();
                        tokio::spawn(async move {
                            let r = znaide_core::llm::openai::probe_models(
                                &base_url,
                                api_key.as_deref(),
                                session_header.as_deref(),
                                &proxy,
                            )
                            .await;
                            let _ = tx.send(WizardReply::Models(r.map_err(|e| format!("{e:#}"))));
                        });
                    }
                    QuickCfgAction::Verify(draft) => {
                        quick_cfg = Some(qc);
                        let tx = wiz_tx.clone();
                        tokio::spawn(async move {
                            let r = znaide_core::llm::openai::probe_chat(&draft).await;
                            let _ = tx.send(WizardReply::Verify(r.map_err(|e| format!("{e:#}"))));
                        });
                    }
                    QuickCfgAction::Save(item) => {
                        // 直接提交:只写目标字段 + 生效四件套,全程不联网
                        match quick_cfg_save(&qc, item, &current_resolved.provider_name, max_turns)
                        {
                            Ok(extra) => {
                                quick_cfg_apply_saved(
                                    &cmd_tx,
                                    &mut current_resolved,
                                    &mut ctx_window,
                                    &mut round_limit,
                                    &mut configured_ok,
                                    &mut items,
                                    max_turns,
                                    &format!("{}已保存并生效。{extra}", item.title()),
                                );
                                qc.inject_saved(item, extra);
                                quick_cfg = Some(qc);
                            }
                            Err(e) => {
                                // 校验失败:停留编辑态,不落盘不生效
                                qc.notice = format!("{e}");
                                quick_cfg = Some(qc);
                            }
                        }
                    }
                }
                continue;
            }
            // 会话管理窗口模式:按键交给窗口,动作由宿主执行
            if let Some(mut su) = sessions_ui.take() {
                match su.on_key(k) {
                    UiAction::Exit => {
                        // 关闭窗口回到对话
                        items.push(MsgItem::Notice(
                            "已关闭会话管理(随时 /resume 再开)。".into(),
                        ));
                    }
                    UiAction::Resume(path) => {
                        let _ = cmd_tx.send(AgentCmd::LoadHistory(path));
                        items.push(MsgItem::Notice("正在恢复历史会话…".into()));
                    }
                    UiAction::None => {
                        sessions_ui = Some(su);
                    }
                }
                continue;
            }
            // 光标移动:Home/End 让给滚动了,这里只管 ←/→/Delete
            match k.code {
                KeyCode::Left => {
                    input_cursor = cursor_prev(&input, input_cursor);
                    continue;
                }
                KeyCode::Right => {
                    input_cursor = cursor_next(&input, input_cursor);
                    continue;
                }
                KeyCode::Delete => {
                    input_cursor = delete_at(&mut input, input_cursor);
                    continue;
                }
                _ => {}
            }
            // 文本输入类按键统一走 handle_text_key(换行/发送/插入语义与单测一致)
            match handle_text_key(k) {
                TextAction::None => {}
                TextAction::Insert(c) => {
                    // 丢弃控制字符(如单独到达的 ESC),防止终端残片混入输入
                    if !c.is_control() {
                        input_cursor = insert_at(&mut input, input_cursor, c);
                    }
                }
                TextAction::Backspace => {
                    input_cursor = backspace_at(&mut input, input_cursor);
                }
                TextAction::Newline => {
                    input_cursor = insert_at(&mut input, input_cursor, '\n');
                }
                TextAction::Submit => {
                    // 发送:非 busy 时执行命令/任务
                    let raw = input.trim().to_string();
                    if raw.is_empty() {
                        input_cursor = 0;
                        continue;
                    }
                    if raw == "/config" {
                        // /config 无参:全量向导,行为零变化(03 §5 互斥:先关按需面板)
                        if quick_cfg.take().is_some() {
                            items.push(MsgItem::Notice(
                                "已关闭按需配置(编辑未保存),打开全量向导…".into(),
                            ));
                        } else {
                            items.push(MsgItem::Notice("打开配置向导…".into()));
                        }
                        let mut w = SetupWizard::new();
                        // 预填现有轮数上限(其余字段在向导里现选/现填)
                        w.prefill();
                        config_wizard = Some(w);
                        input.clear();
                        input_cursor = 0;
                    } else if raw == "/cfg"
                        || raw.starts_with("/cfg ")
                        || raw.starts_with("/config ")
                    {
                        // need03:按需直改面板。/cfg[/cfg <子项>] 与 /config <子项> 进同一面板;
                        // /config 无参仍走全向导。首次运行(无可用配置)拒绝进入,指引 /config。
                        if config_wizard.take().is_some() {
                            items.push(MsgItem::Notice(
                                "已关闭全量向导(编辑未保存),打开按需配置…".into(),
                            ));
                        }
                        if !configured_ok {
                            items.push(MsgItem::Notice(
                                "⚠ 还没有任何可用配置,先用 /config 完成一次初始化,再用 /cfg 按需改。"
                                    .into(),
                            ));
                            input.clear();
                            input_cursor = 0;
                        } else {
                            let arg = if raw == "/cfg" {
                                String::new()
                            } else if let Some(a) = raw.strip_prefix("/cfg ") {
                                a.to_string()
                            } else {
                                raw.strip_prefix("/config ").unwrap_or("").to_string()
                            };
                            let arg = arg.trim().to_string();
                            if arg.is_empty() {
                                quick_cfg = Some(QuickCfgPanel::open_menu());
                                items.push(MsgItem::Notice("打开按需配置…".into()));
                            } else if let Some(item) = parse_qc_alias(&arg) {
                                quick_cfg = Some(QuickCfgPanel::open_with(item));
                                items.push(MsgItem::Notice(format!(
                                    "打开按需配置:{}(s 直接提交 / v 验证后提交)…",
                                    item.title()
                                )));
                            } else {
                                let mut p = QuickCfgPanel::open_menu();
                                p.notice =
                                    format!("未知子项 {arg},请从菜单选择。");
                                quick_cfg = Some(p);
                                items.push(MsgItem::Notice("打开按需配置…".into()));
                            }
                            input.clear();
                            input_cursor = 0;
                        }
                    } else if raw.starts_with('/') {
                        match handle_command(
                            &raw,
                            &cmd_tx,
                            &update_tx,
                            &mut items,
                            &cwd,
                            &mut confirm,
                        ) {
                            Some(SlashOutcome::Task(prompt)) => {
                                if !configured_ok {
                                    items.push(MsgItem::Notice(
                                        "⚠ 尚未完成配置,不能执行任务。先输入 /config 完成配置。"
                                            .into(),
                                    ));
                                } else {
                                    items.push(MsgItem::User(prompt.clone()));
                                    let _ = cmd_tx.send(AgentCmd::Prompt(prompt));
                                    busy = true;
                                    at_bottom = true;
                                }
                            }
                            Some(SlashOutcome::RunSkill { name, args }) => {
                                if !configured_ok {
                                    items.push(MsgItem::Notice(
                                        "⚠ 尚未完成配置,不能执行技能。先输入 /config 完成配置。"
                                            .into(),
                                    ));
                                } else {
                                    let _ = cmd_tx.send(AgentCmd::RunSkill { name, args });
                                    busy = true;
                                    at_bottom = true;
                                }
                            }
                            Some(SlashOutcome::Compact) => {
                                if !configured_ok {
                                    items.push(MsgItem::Notice(
                                        "⚠ 尚未完成配置,不能压缩上下文。先输入 /config 完成配置。"
                                            .into(),
                                    ));
                                } else {
                                    let _ = cmd_tx.send(AgentCmd::Compact);
                                }
                            }
                            Some(SlashOutcome::Quit) => {
                                // /quit /exit:退出主循环(Ctrl+C 等效);退出后打印会话统计
                                quit_requested = true;
                            }
                            Some(SlashOutcome::OpenSessions) => {
                                // /resume 无参:打开全屏会话管理窗口
                                sessions_ui = Some(SessionsUi::open(&session_id));
                            }
                            None => {}
                        }
                        input.clear();
                        input_cursor = 0;
                    } else if busy {
                        // 工作中不丢弃:入队,会话引擎会在下一个轮边界把它插进对话
                        // (以前是静默忽略:用户打了字按 Enter 毫无反应)
                        items.push(MsgItem::Notice(queue.push(&raw)));
                        input.clear();
                        input_cursor = 0;
                    } else if !configured_ok {
                        items.push(MsgItem::Notice(
                            "⚠ 尚未完成配置,不能对话。先输入 /config 完成配置。".into(),
                        ));
                        input.clear();
                        input_cursor = 0;
                    } else {
                        items.push(MsgItem::User(raw.clone()));
                        let _ = cmd_tx.send(AgentCmd::Prompt(raw));
                        busy = true;
                        at_bottom = true;
                        input.clear();
                        input_cursor = 0;
                    }
                }
            }
            // 全局按键(Esc 中断 / 方向键滚动)
            match k.code {
                KeyCode::Esc => {
                    if busy {
                        if let Some(tok) = shared_cancel.lock().unwrap().as_ref() {
                            tok.cancel();
                        }
                        items.push(MsgItem::Notice("⏹ 已请求中断…".into()));
                    }
                }
                KeyCode::Up => {
                    // 上滚一屏的几分之一,离开底部
                    at_bottom = false;
                    scroll_back = scroll_back.saturating_add(3);
                }
                KeyCode::Down => {
                    scroll_back = scroll_back.saturating_sub(3);
                    if scroll_back == 0 {
                        at_bottom = true;
                    }
                }
                KeyCode::PageUp => {
                    at_bottom = false;
                    scroll_back = scroll_back.saturating_add(15);
                }
                KeyCode::PageDown => {
                    scroll_back = scroll_back.saturating_sub(15);
                    if scroll_back == 0 {
                        at_bottom = true;
                    }
                }
                KeyCode::Home => {
                    // 回到最顶:偏移尽量大(渲染时夹取)
                    at_bottom = false;
                    scroll_back = usize::MAX;
                }
                KeyCode::End => {
                    scroll_back = 0;
                    at_bottom = true;
                }
                KeyCode::BackTab => {
                    // Shift+Tab:循环切换权限模式
                    cycle_permission_mode(&mut current_mode, &cmd_tx, &mut items);
                }
                KeyCode::Tab => {
                    // 部分终端把 Shift+Tab 报成 Tab+SHIFT(而非 BackTab)
                    if k.modifiers.contains(KeyModifiers::SHIFT) {
                        cycle_permission_mode(&mut current_mode, &cmd_tx, &mut items);
                    }
                }
                _ => {}
            }
        }

        // 配置向导异步结果回填
        loop {
            match wiz_rx.try_recv() {
                Ok(WizardReply::Models(r)) => {
                    // 按需面板与全向导互斥:谁在谁吃
                    if let Some(q) = &mut quick_cfg {
                        q.inject_models(r);
                    } else if let Some(w) = &mut config_wizard {
                        w.inject_models(r);
                    }
                }
                Ok(WizardReply::Verify(r)) => {
                    // 按需面板 Verifying 态优先:成功才落盘+生效,失败停留可重试(02 §4)
                    let qc_verifying = match &quick_cfg {
                        Some(q) => match q.step {
                            QcStep::Verifying { item } => Some(item),
                            _ => None,
                        },
                        None => None,
                    };
                    if let (Some(item), Some(q)) = (qc_verifying, &mut quick_cfg) {
                        match r {
                            Ok(()) => {
                                match quick_cfg_save(
                                    q,
                                    item,
                                    &current_resolved.provider_name,
                                    max_turns,
                                ) {
                                    Ok(extra) => {
                                        quick_cfg_apply_saved(
                                            &cmd_tx,
                                            &mut current_resolved,
                                            &mut ctx_window,
                                            &mut round_limit,
                                            &mut configured_ok,
                                            &mut items,
                                            max_turns,
                                            &format!(
                                                "验证通过:{}已保存并生效。{extra}",
                                                item.title()
                                            ),
                                        );
                                        q.inject_verify_ok(item, extra);
                                    }
                                    Err(e) => {
                                        q.inject_verify_err(item, format!("落盘失败:{e}"));
                                    }
                                }
                            }
                            Err(e) => {
                                q.inject_verify_err(item, e);
                            }
                        }
                    } else if let Some(w) = &mut config_wizard {
                        match r {
                            Ok(()) => {
                                // 验证通过 → 保存并解锁
                                let draft = w.draft();
                                let mut cfg =
                                    znaide_core::config::Config::load().unwrap_or_default();
                                // 面板里填的轮数上限一起落盘(空 = 清除,回到默认 200)
                                cfg.max_turns = w.max_turns;
                                // 弱网重试一起落盘(顶层 retry，默认关闭)
                                cfg.retry = w.retry;
                                // 网络代理一起落盘(顶层 proxy，默认跟随环境；save() 7 参不动)
                                cfg.proxy = w.proxy.clone();
                                // Key 来自环境变量时不写明文(留空即保持环境变量那条路)
                                let key_arg = if w.key_from_env {
                                    None
                                } else {
                                    draft.api_key.as_deref()
                                };
                                let save_result = cfg.save(
                                    &draft.provider_name,
                                    Some(&draft.model),
                                    Some(&draft.base_url),
                                    key_arg,
                                    w.context_window,
                                    draft.protocol,
                                    w.session_header,
                                );
                                // 协议变了 → 提醒历史继续有效、异常可 /clear
                                let proto_note = if current_resolved.protocol != draft.protocol {
                                    format!(
                                        "协议已切换为 {}，历史继续有效；若模型表现异常可 /clear 开新上下文。",
                                        draft.protocol.as_str()
                                    )
                                } else {
                                    String::new()
                                };
                                // 状态栏立即跟上新的轮数上限(命令行 --max-turns 仍优先)
                                round_limit = effective_max_turns(max_turns);
                                let _ = cmd_tx.send(AgentCmd::Reconfigure(draft.clone()));
                                current_resolved = draft.clone();
                                // 配置变了,窗口跟着刷新(状态栏 ctx 条用的就是它)
                                ctx_window = current_resolved.effective_context_window();
                                configured_ok = save_result.is_ok();
                                w.inject_verify(
                                    true,
                                    format!(
                                        " {} / {} 已保存并生效。",
                                        draft.provider_name, draft.model
                                    ),
                                );
                                items.push(MsgItem::Notice(match save_result {
                                    Ok(()) => format!(
                                        "✔ 配置完成并已生效: {} / {}。可以开始用了。{proto_note}",
                                        draft.provider_name, draft.model
                                    ),
                                    Err(e) => format!("⚠ 配置已生效但没存上: {e}"),
                                }));
                                // 验证通过后自动关掉向导回到对话
                                config_wizard = None;
                            }
                            Err(e) => {
                                if let Some(w) = &mut config_wizard {
                                    w.inject_verify(false, e);
                                }
                            }
                        }
                    }
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
            }
        }
        // 更新通知(启动自动检查 / /update 的后台结果)
        loop {
            match update_rx.try_recv() {
                Ok(text) => items.push(MsgItem::Notice(text)),
                Err(_) => break,
            }
        }
        // agent 事件
        loop {
            match ev_rx.try_recv() {
                Ok(ev) => {
                    match ev {
                        // 轮次进度:只更新状态栏,不进消息流
                        SessionEvent::RoundStarted { used, limit } => {
                            round_used = used;
                            round_limit = limit;
                        }
                        // 轮次预算用尽:用确认框问用户要不要再放一批(引擎在等回答)
                        SessionEvent::RoundsExhausted { used, tx } => {
                            confirm = Some(ConfirmBox {
                                title: "继续执行".into(),
                                body: format!(
                                    "已达 {used} 轮工具调用上限,任务还没干完。\n\
                                     再放一批继续跑吗?(想一次跑更久:启动加 --max-turns N,\
                                     或在 config.json 里设 max_turns,0 = 不限)"
                                ),
                                action: ConfirmAction::ContinueRounds(tx),
                            });
                        }
                        other => handle_session_event(
                            other,
                            &mut items,
                            &mut busy,
                            &mut busy_kind,
                            &mut permission,
                            &mut session_id,
                            &mut persona_disp,
                            &mut token_stats,
                        ),
                    }
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    anyhow::bail!("会话引擎已退出");
                }
            }
        }
        // 队列兜底放行:正常情况下回合执行中排的消息由**引擎在轮边界**取走(忙态不变);
        // 只有"这一回合已无后续轮次"(模型给出终结答复)时,剩下的才落到这里——
        // 回合结束(busy → false)且没有模态弹窗时,逐条当作新回合发出,一条一个回合。
        // 自然结束、被 Esc 中断、回合出错中止都落在同一个 busy=false 上,队列不会卡死。
        let modal = permission.is_some()
            || confirm.is_some()
            || config_wizard.is_some()
            || quick_cfg.is_some()
            || sessions_ui.is_some();
        if let Some(next) = queue.pop_ready(!busy, modal) {
            items.push(MsgItem::User(next.clone()));
            let _ = cmd_tx.send(AgentCmd::Prompt(next));
            busy = true;
            busy_kind = BusyKind::Work;
            at_bottom = true;
        }
        // 输入补全刷新:基于最新 input/光标;前缀与位置未变时保留选中,不做磁盘 IO。
        // 向导/确认框/工作中时不触发(相关层已拦截按键,这里兜底清空状态)。
        let completable = !busy
            && permission.is_none()
            && confirm.is_none()
            && config_wizard.is_none()
            && quick_cfg.is_none();
        refresh_completion(&input, input_cursor, cwd, completable, &mut completion);
        // 上下文占用警示:空闲时 >=90% 提示一次(可 /compact 或开新会话);回落后重新武装。
        // 窗口未知不提示(宁可不说,也不拿猜出来的窗口劝人做没必要的压缩);
        // 占用还没测到(None)也不提示——resume 完第一轮的占用恰恰是未知的。
        if !busy {
            if let Some(win) = ctx_window.filter(|w| *w > 0) {
                if let Some(used) = token_stats.last_prompt.filter(|u| *u > 0) {
                    let pct = (used as u128) * 100 / (win as u128);
                    if pct >= 90 && !ctx_warn_shown {
                        items.push(MsgItem::Notice(format!(
                            "⚠ 上下文已用约 {pct}%,再聊可能丢最早的记录。\
                             可以 /compact 压缩,或 /quit 开新会话(--resume 能找回完整历史)。"
                        )));
                        ctx_warn_shown = true;
                    } else if pct < 90 {
                        ctx_warn_shown = false;
                    }
                }
            }
        }
        // /quit、/exit(Ctrl+C 在按键处 break)请求退出
        if quit_requested {
            break;
        }
    }

    agent_task.abort();
    {
        // 与启动镜像反序还原(Windows 上 crossterm 的 console mode 快照是链式的)
        let _ = disable_mouse_capture();
        let mut so = std::io::stdout();
        let _ = execute!(
            so,
            crossterm::event::DisableBracketedPaste,
            LeaveAlternateScreen
        );
    }
    disable_raw_mode()?;
    // 收集本次会话统计(供 CLI 打印)
    let stats = collect_exit_stats(
        &items,
        &session_id,
        &token_stats,
        app_start.elapsed().as_secs(),
    );
    Ok(stats)
}

/// 汇总本次交互会话的统计(退出时展示)
fn collect_exit_stats(
    items: &[MsgItem],
    session_id: &str,
    tokens: &TokenStats,
    uptime_secs: u64,
) -> ExitStats {
    let mut user_msgs = 0u32;
    let mut assistant_msgs = 0u32;
    let mut tool_calls = 0u32;
    let mut shell_calls = 0u32;
    let mut file_writes = 0u32;
    for it in items {
        match it {
            MsgItem::User(_) => user_msgs += 1,
            MsgItem::Assistant(_) | MsgItem::AssistantStream(_) => assistant_msgs += 1,
            MsgItem::Tool { name, ok, .. } if *ok == Some(true) => {
                tool_calls += 1;
                match name.as_str() {
                    "run_shell_command" => shell_calls += 1,
                    "write_file" | "edit" | "skill" => file_writes += 1,
                    _ => {}
                }
            }
            _ => {}
        }
    }
    let history = if session_id.is_empty() {
        None
    } else {
        // 懒创建:没对话的会话本就没有历史文件,只在文件真实存在时展示
        let p = znaide_core::config::data_dir()
            .join("sessions")
            .join(format!("{session_id}.jsonl"));
        if p.exists() {
            Some(p.display().to_string())
        } else {
            None
        }
    };
    let undo_snapshots = if session_id.is_empty() {
        0
    } else {
        znaide_core::undo::list_session(session_id).len()
    };
    ExitStats {
        session_id: session_id.to_string(),
        history,
        uptime_secs,
        user_msgs,
        assistant_msgs,
        tool_calls,
        shell_calls,
        file_writes,
        tokens_in: tokens.input,
        tokens_out: tokens.output,
        undo_snapshots,
    }
}

/// 按序号(1 起)或文件名片段匹配历史会话
fn match_session(sessions: &[PathBuf], target: &str) -> Option<PathBuf> {
    if let Ok(idx) = target.parse::<usize>() {
        return sessions.get(idx.saturating_sub(1)).cloned();
    }
    sessions
        .iter()
        .find(|p| {
            p.to_string_lossy().contains(target)
                || p.file_stem()
                    .map(|s| s.to_string_lossy().contains(target))
                    .unwrap_or(false)
        })
        .cloned()
}

/// slash 命令处理结果
enum SlashOutcome {
    /// 需作为新回合发给 agent 的普通指令(技能无入口时,正文即指令)
    Task(String),
    /// 需 agent 侧执行技能(可能有入口脚本,权限与工具事件在 agent 内统一处理)
    RunSkill { name: String, args: String },
    /// 压缩上下文(旧消息 → 摘要)
    Compact,
    /// 退出程序(/quit /exit;退出时打印本次会话统计)
    Quit,
    /// 打开全屏会话管理窗口(/resume 无参)
    OpenSessions,
}

/// 处理 slash 命令。返回 Some(结果) 表示应作为任务/技能发给 agent;None 表示内部处理。
fn handle_command(
    cmd: &str,
    cmd_tx: &mpsc::UnboundedSender<AgentCmd>,
    update_tx: &mpsc::UnboundedSender<String>,
    items: &mut Vec<MsgItem>,
    cwd: &Path,
    confirm: &mut Option<ConfirmBox>,
) -> Option<SlashOutcome> {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    let name = parts.first().copied().unwrap_or("");
    match name {
        "/help" => {
            items.push(MsgItem::CommandOutput(HELP_TEXT.to_string()));
        }
        "/clear" => {
            // 破坏性操作:先弹确认,y 才真正清空
            *confirm = Some(ConfirmBox {
                title: "清空会话?".into(),
                body: "将清空本会话全部上下文(模型记忆)与历史文件(jsonl),不可恢复。\n清空后继续对话不再带此前任何内容,重要内容先自己保存。".into(),
                action: ConfirmAction::ClearSession,
            });
        }
        "/skills" => {
            let txt = skill_list_text(cwd);
            if txt.is_empty() {
                items.push(MsgItem::CommandOutput(
                    "暂无已安装技能。\n技能目录:~/.znaide/skills/ 用户级, .znaide/skills/ 项目级"
                        .into(),
                ));
            } else {
                items.push(MsgItem::CommandOutput(txt));
            }
        }
        "/undo" => {
            if parts.len() >= 2 {
                if let Ok(idx) = parts[1].parse::<usize>() {
                    match znaide_core::undo::rollback_by_index(idx) {
                        Ok(snap) => {
                            items.push(MsgItem::Notice(format!(
                                "已回滚 #{idx}: {} ← 快照 {}",
                                snap.orig, snap.ts_ms
                            )));
                        }
                        Err(e) => items.push(MsgItem::CommandOutput(format!("回滚失败: {e}"))),
                    }
                } else {
                    items.push(MsgItem::CommandOutput(
                        "用法:/undo(列表) 或 /undo <序号>(回滚)".into(),
                    ));
                }
                return None;
            }
            let snaps = znaide_core::undo::list();
            if snaps.is_empty() {
                items.push(MsgItem::CommandOutput(
                    "暂无 undo 快照(还没有文件被修改过)".into(),
                ));
                return None;
            }
            let mut out = String::from(
                "undo 快照(最新在前,共 ".to_owned() + &snaps.len().to_string() + "):\n",
            );
            for (i, s) in snaps.iter().take(20).enumerate() {
                let t = s.ts_ms as f64 / 1000.0;
                let ext = if s.is_external() { " ·外部" } else { "" };
                out.push_str(&format!(
                    "  {}. {} — {} ({}){}\n",
                    i + 1,
                    s.action,
                    s.orig,
                    fmt_time(t),
                    ext
                ));
            }
            if snaps.len() > 20 {
                out.push_str(&format!("…(共 {} 条,最新 20 条)\n", snaps.len()));
            }
            items.push(MsgItem::CommandOutput(out));
        }
        "/resume" => {
            // 无参:打开全屏会话管理窗口(会话 / 记忆两个页签,多选批量删除、
            // 备注编辑、过滤、恢复;Esc 返回)
            if parts.len() == 1 {
                return Some(SlashOutcome::OpenSessions);
            }
            // 带参数:恢复 / 删除(兼容旧行为,脚本友好)
            let history = list_history_sessions_detailed();
            if history.is_empty() {
                items.push(MsgItem::CommandOutput(
                    "暂无历史会话(~/.znaide/sessions/ 为空)".into(),
                ));
                return None;
            }
            let paths: Vec<PathBuf> = history.iter().map(|h| h.path.clone()).collect();
            if parts[1] == "del" {
                // /resume del <序号|片段> 删除历史
                if parts.len() < 3 {
                    items.push(MsgItem::CommandOutput(
                        "用法:/resume del <序号|文件名片段>".into(),
                    ));
                    return None;
                }
                let target = parts[2];
                let matched = match_session(&paths, target);
                match matched {
                    Some(path) => match remove_session(&path) {
                        Ok(()) => {
                            items.push(MsgItem::Notice(format!(
                                "🗑 已删除历史会话: {}",
                                path.display()
                            )));
                        }
                        Err(e) => items.push(MsgItem::CommandOutput(format!("删除失败: {e}"))),
                    },
                    None => {
                        items.push(MsgItem::CommandOutput(format!(
                            "未找到匹配「{target}」的历史会话"
                        )));
                    }
                }
                return None;
            }
            // 尝试:传入序号或文件路径 → 恢复
            let target = parts[1];
            let matched = match_session(&paths, target);
            match matched {
                Some(path) => {
                    let _ = cmd_tx.send(AgentCmd::LoadHistory(path));
                    items.push(MsgItem::Notice("正在恢复历史会话…".into()));
                }
                None => {
                    items.push(MsgItem::CommandOutput(format!(
                        "未找到匹配「{target}」的历史会话"
                    )));
                }
            }
            return None;
        }
        "/compact" => {
            items.push(MsgItem::Notice("▶ 请求压缩上下文…".into()));
            return Some(SlashOutcome::Compact);
        }
        "/quit" | "/exit" => {
            items.push(MsgItem::Notice("正在退出…".into()));
            return Some(SlashOutcome::Quit);
        }
        "/update" => {
            // 后台执行(下载可能要一阵),进度/结果经 update 通道回 UI
            items.push(MsgItem::Notice(
                "正在检查更新…(Gitee 优先,不通自动切 GitHub)".into(),
            ));
            let tx = update_tx.clone();
            // 生效代理现算（文件值 + ENV；handle_command 拿不到 current_resolved，
            // 但 /cfg 保存后已落盘，现算结果与 current_resolved.proxy 一致）
            let proxy_url: Option<String> = znaide_core::config::Config::load()
                .ok()
                .map(|c| c.resolve_proxy(None, false))
                .and_then(|p| p.url().map(|s| s.to_string()));
            tokio::spawn(async move {
                let cur = znaide_core::update::current_version();
                let msg = znaide_core::update::perform_update_with_proxy(proxy_url.as_deref())
                    .await
                    .describe(&cur);
                let _ = tx.send(msg);
            });
        }
        "/persona" => {
            let avail = znaide_core::persona::list();
            let current = znaide_core::config::Config::load()
                .ok()
                .and_then(|c| c.persona)
                .unwrap_or_default();
            let cur_label = if current.is_empty() {
                "无(默认助手)".to_string()
            } else {
                format!("「{current}」")
            };
            if parts.len() >= 2 {
                let target = parts[1];
                let known = target == "无" || avail.iter().any(|n| n == target);
                if !known {
                    let hint = if avail.is_empty() {
                        "无".to_string()
                    } else {
                        format!("{}、无", avail.join("、"))
                    };
                    items.push(MsgItem::CommandOutput(format!(
                        "没有这个人格:「{target}」。可用:{hint}"
                    )));
                    return None;
                }
                let _ = cmd_tx.send(AgentCmd::SetPersona(target.to_string()));
                items.push(MsgItem::Notice(format!(
                    "正在切换人格「{target}」…(重建系统提示,之后所有对话都带着它)"
                )));
                return None;
            }
            let mut out = format!("人格设置(当前:{cur_label},全局生效)\n\n可用人格:\n");
            for n in &avail {
                out.push_str(&format!("  /persona {n}\n"));
            }
            out.push_str("  /persona 无   关闭人格,恢复默认助手\n\n自定义:往 ~/.znaide/personas/ 放一个 <名字>.md(正文即人设说明),下次启动即可用 /persona 选它");
            items.push(MsgItem::CommandOutput(out));
        }
        _ => {
            // 不是内置命令:当技能 /名字 手动触发(技能目录 ~/.znaide/skills/ 或 .znaide/skills/)
            let cmd_name = name.trim_start_matches('/');
            let set = znaide_core::skills::scan(cwd);
            match znaide_core::skills::find(&set.skills, cmd_name) {
                Some(skill) => {
                    let args = parts[1..].join(" ");
                    items.push(MsgItem::Notice(format!(
                        "▶ 技能「{}」已触发: {}({})",
                        skill.name,
                        skill.description,
                        skill.source.label()
                    )));
                    if skill.entry.is_some() {
                        items.push(MsgItem::Notice(
                            "该技能声明了入口脚本,执行时可能需要你确认。".into(),
                        ));
                        return Some(SlashOutcome::RunSkill {
                            name: skill.name.clone(),
                            args,
                        });
                    }
                    // 无入口:正文即本回合指令(旧自定义命令同款行为)
                    let prompt = skill.render(&args);
                    return Some(SlashOutcome::Task(prompt));
                }
                None => {
                    // 输入的不是内置命令,也不是已安装技能
                    let hint = if set.skills.is_empty() {
                        String::new()
                    } else {
                        "\n输入 /skills 可查看已安装技能".to_string()
                    };
                    items.push(MsgItem::CommandOutput(format!(
                        "未知命令: {name}\n输入 /help 查看内置命令{hint}"
                    )));
                }
            }
        }
    }
    None
}

/// 列出已安装技能(供 /skills 展示);附带扫描告警,便于发现坏 SKILL.md
fn skill_list_text(cwd: &Path) -> String {
    let set = znaide_core::skills::scan(cwd);
    if set.skills.is_empty() && set.warnings.is_empty() {
        return String::new();
    }
    let mut out = String::from("已安装技能(直接输入 /名字 触发;模型也可自主调用):\n");
    for s in &set.skills {
        let flag = if s.disable_model_invocation {
            " [仅手动]"
        } else {
            ""
        };
        out.push_str(&format!(
            "  /{} — {}({}){}\n",
            s.name,
            s.description,
            s.source.label(),
            flag
        ));
    }
    if set.skills.is_empty() {
        out.push_str("  (暂无可用技能)\n");
    }
    for w in &set.warnings {
        out.push_str(&format!("  ⚠ {w}\n"));
    }
    out.push_str("(技能目录:~/.znaide/skills/ 用户级, .znaide/skills/ 项目级)");
    out
}

const HELP_TEXT: &str = "可用命令:
  /help                    显示本帮助
  /skills                  列出已安装技能
  /config                  打开配置面板(随时修改 provider/模型/端点/key/轮数上限,立即生效)
  /cfg                     按需改配置(选一项改一项,可直接提交/验证后提交;/cfg <子项> 直达)
  /undo                    列出 undo 快照; /undo <序号> 回滚
  /resume                  打开会话管理窗口(会话/记忆页签,多选批量删、备注、恢复); /resume <序号|片段> 恢复; /resume del <序号|片段> 删除
  /clear                   清空当前会话上下文与历史文件(需确认)
  /compact                 压缩上下文:旧对话收敛为摘要,腾出空间继续对话
  /update                  检查并更新到 GitHub 最新版本
  /persona                 人格设置:/persona 列表, /persona <名字> 切换, /persona 无 关闭
  /技能名                  触发已安装技能(列表见 /skills)

输入技巧:
  @文件 或 @目录          引用本地文件/目录(自动注入内容)
  Enter                   发送
  Shift+Enter 换行        若终端不支持,可用 Alt+Enter 或 Ctrl+J
  Shift+Tab               循环切换权限模式(询问 → 编辑放行 → 全自动 → 超级)
  Shift+拖动              选择并复制文字(鼠标已用于滚轮/滚动条,按住 Shift 可拖选)
  Esc                     中断生成 / 取消
  Ctrl+C                  退出

权限模式说明(Shift+Tab 循环切换;输入框与消息区边框颜色随模式变化):
  询问       写文件、执行命令前均需你确认(默认,边框绿色)
  编辑放行   文件修改自动放行,命令执行仍确认(边框天蓝)
  全自动     全部自动执行,高危命令仍要人工确认(边框紫色)
  超级 YOLO  一切放行、无任何问询,高危判定也跳过——高度危险,后果自负(边框红色)
";

/// 把一条历史消息转成 UI 展示条目(供 /resume 恢复后展示)
fn push_history_item(
    items: &mut Vec<MsgItem>,
    msg: &znaide_core::llm::types::ChatMessage,
    tool_names: &mut std::collections::HashMap<String, String>,
    pending: &mut std::collections::HashMap<String, usize>,
) {
    use znaide_core::llm::types::Role;
    match msg.role {
        Role::System => {}
        Role::User => {
            if let Some(c) = msg.content.as_deref().filter(|c| !c.is_empty()) {
                items.push(MsgItem::User(c.to_string()));
            }
        }
        Role::Assistant => {
            if let Some(c) = msg.content.as_deref().filter(|c| !c.is_empty()) {
                items.push(MsgItem::Assistant(c.to_string()));
            }
            if let Some(calls) = &msg.tool_calls {
                for tc in calls {
                    tool_names.insert(tc.id.clone(), tc.function.name.clone());
                    // 与实时一致的卡片:等后续 tool 结果消息回填
                    items.push(MsgItem::Tool {
                        name: tc.function.name.clone(),
                        args: tc.function.arguments.clone(),
                        ok: None,
                        output: String::new(),
                        started: None,
                        done_secs: None,
                        idle_ms: None,
                        silent: 0,
                        live: String::new(),
                    });
                    pending.insert(tc.id.clone(), items.len() - 1);
                }
            }
        }
        Role::Tool => {
            let content = msg.content.clone().unwrap_or_default();
            let idx = msg
                .tool_call_id
                .as_deref()
                .and_then(|id| pending.remove(id));
            match idx {
                Some(i) => {
                    if let Some(MsgItem::Tool { ok, output, .. }) = items.get_mut(i) {
                        *ok = Some(true);
                        *output = content;
                    }
                }
                // 找不到对应声明的结果消息(极端情况):单独成卡
                None => {
                    let name = msg
                        .tool_call_id
                        .as_deref()
                        .and_then(|id| tool_names.get(id).cloned())
                        .unwrap_or_else(|| "工具结果".to_string());
                    items.push(MsgItem::Tool {
                        name,
                        args: String::new(),
                        ok: Some(true),
                        output: cap_tool_output(content),
                        started: None,
                        done_secs: None,
                        idle_ms: None,
                        silent: 0,
                        live: String::new(),
                    });
                }
            }
        }
    }
}

/// token 统计(会话级):input/output = 服务端真实 usage 累计(端点不返回就 0);
/// last_prompt = 最近一次请求的真实 prompt(即当前上下文占用)。
/// **默认 `Some(0)`**:新会话、`/clear` 之后,上下文里确实没有对话,画 0% 是对的。
/// "**还没测到**"必须是显式的 `None`——只有刚 `/resume` / `/compact` 完是这样:
/// 恢复出来的历史、压缩后的摘要都实打实占着上下文,画成 0% 会让人以为还能再塞很多。
/// round_est = 当前回复的流式估算,该轮真实 usage 到账后清零。
struct TokenStats {
    input: u64,
    output: u64,
    last_prompt: Option<u64>,
    round_est: u64,
}

impl Default for TokenStats {
    fn default() -> Self {
        Self {
            input: 0,
            output: 0,
            last_prompt: Some(0),
            round_est: 0,
        }
    }
}

fn handle_session_event(
    ev: SessionEvent,
    items: &mut Vec<MsgItem>,
    busy: &mut bool,
    busy_kind: &mut BusyKind,
    permission: &mut Option<PermissionPrompt>,
    session_id: &mut String,
    persona: &mut String,
    stats: &mut TokenStats,
) {
    match ev {
        SessionEvent::TurnStarted => {
            *busy = true;
            *busy_kind = BusyKind::Work;
            // 新回合:进行中回复的估算清零
            stats.round_est = 0;
        }
        SessionEvent::ReasoningDelta(t) => {
            stats.round_est += est_tokens(&t);
            match items.last_mut() {
                Some(MsgItem::Reasoning(r)) => r.push_str(&t),
                _ => items.push(MsgItem::Reasoning(t)),
            }
        }
        SessionEvent::TextDelta(t) => {
            stats.round_est += est_tokens(&t);
            match items.last_mut() {
                Some(MsgItem::AssistantStream(s)) => s.push_str(&t),
                _ => items.push(MsgItem::AssistantStream(t)),
            }
        }
        SessionEvent::LlmRetrying { attempt, max, reason } => {
            // 弱网重试：丢弃本轮已吐出的部分流式内容，只保留最终成功的一次。
            // 本轮前面已落定的工具卡片/历史不受影响：只从尾部向前清掉连续的
            // AssistantStream/Reasoning（当前 attempt 的半截输出），停在第一张非流式卡片。
            while matches!(
                items.last(),
                Some(MsgItem::AssistantStream(_) | MsgItem::Reasoning(_))
            ) {
                items.pop();
            }
            stats.round_est = 0;
            items.push(MsgItem::Notice(format!(
                "↻ 弱网重试 {attempt}/{max}（{reason}），已丢弃部分输出…"
            )));
        }
        SessionEvent::Usage { prompt, completion } => {
            // 该轮真实用量到账:输入/输出分别累计,进行中估算清零(已按真实记账)
            stats.input += prompt;
            stats.output += completion;
            stats.last_prompt = Some(prompt); // 当前上下文占用 = 最近一次请求的 prompt
            stats.round_est = 0;
        }
        SessionEvent::ToolStarted {
            name,
            args,
            idle_ms,
        } => {
            items.push(MsgItem::Tool {
                name,
                args,
                ok: None,
                output: String::new(),
                started: Some(Instant::now()),
                done_secs: None,
                idle_ms,
                silent: 0,
                live: String::new(),
            });
        }
        SessionEvent::ToolFinished { name, ok, output } => {
            if let Some(MsgItem::Tool {
                ok: slot,
                output: out,
                started,
                done_secs,
                live,
                ..
            }) = items
                .iter_mut()
                .rev()
                .find(|i| matches!(i, MsgItem::Tool { name: n, .. } if *n == name))
            {
                *slot = Some(ok);
                *out = cap_tool_output(output);
                // 实时输出区使命结束:清空,卡片高度回落
                live.clear();
                // 定格总用时:结束那一瞬的 elapsed,之后渲染不再跳动
                if let Some(st) = started {
                    *done_secs = Some(st.elapsed().as_secs());
                }
            }
        }
        SessionEvent::ToolOutputDelta { name, delta } => {
            // 追加到执行中(ok=None)的同名卡片;找不到(已结束/历史)忽略
            if let Some(MsgItem::Tool { ok: None, live, .. }) = items
                .iter_mut()
                .rev()
                .find(|i| matches!(i, MsgItem::Tool { name: n, ok: None, .. } if *n == name))
            {
                live.push_str(&delta);
                cap_live_tail(live);
            }
        }
        SessionEvent::ToolSilentAlert { name, level } => {
            // 只给执行中(ok=None)的同名卡片打档;找不到(如非命令卡片)忽略
            if let Some(MsgItem::Tool { silent: slot, .. }) = items
                .iter_mut()
                .rev()
                .find(|i| matches!(i, MsgItem::Tool { name: n, ok: None, .. } if *n == name))
            {
                *slot = level;
            }
        }
        SessionEvent::PersonaChanged { name } => {
            // 状态栏人格显示跟随切换(空 = 关闭人格)
            *persona = name;
        }
        SessionEvent::PermissionRequest {
            title,
            body,
            warning,
            tx,
            ..
        } => {
            *permission = Some(PermissionPrompt {
                title,
                body,
                warning,
                tx,
            });
        }
        SessionEvent::RoundsExhausted { .. } => {
            // 正常路径下由事件循环拦截并弹"继续执行"确认框(见 run 的事件泵);
            // 走到这里说明没人接管,直接丢弃发送端 = 引擎按"不续跑"收尾。
        }
        SessionEvent::RoundStarted { .. } => {
            // 同上:状态栏用的轮次进度由事件泵直接消费,不进消息流
        }
        SessionEvent::UserInjected { text } => {
            // 排队消息已在轮边界插进对话:补一张"你:"卡片(忙态不变,这一轮还在跑)
            items.push(MsgItem::User(text));
        }
        SessionEvent::Notice(t) => {
            items.push(MsgItem::Notice(t));
        }
        SessionEvent::ContextCleared => {
            // /clear 确认后由 agent 侧回执:上下文与历史文件已清空,同步清空展示
            items.clear();
            items.push(MsgItem::Notice(
                "✔ 已清空会话上下文与历史文件,可重新开始。".into(),
            ));
            // 历史文件都清了 = 换了个会话:占用与累计用量一起归零
            // (累计 in/out 不再残留上一个会话的数字;ctx 回到"默认的空"= 0%,见 TokenStats)
            *stats = TokenStats::default();
        }
        SessionEvent::CompactionStarted => {
            // 压缩是耗时操作:置忙(收纳推进条动画由忙行动画呈现),不再 push 静态提示
            *busy = true;
            *busy_kind = BusyKind::Compacting;
        }
        SessionEvent::ContextCompacted {
            removed,
            summary,
            kept,
        } => {
            // /compact 完成:旧对话收起为摘要,保留最近消息的展示;结束忙态
            *busy = false;
            *busy_kind = BusyKind::Work;
            items.clear();
            items.push(MsgItem::Notice(format!(
                "✔ 上下文已压缩:此前 {removed} 条旧消息收敛为一段摘要"
            )));
            items.push(MsgItem::CommandOutput(summary));
            if !kept.is_empty() {
                items.push(MsgItem::Notice(format!("保留最近 {} 条消息:", kept.len())));
                let mut tool_names = std::collections::HashMap::new();
                let mut pending = std::collections::HashMap::new();
                for m in &kept {
                    push_history_item(items, m, &mut tool_names, &mut pending);
                }
            }
            // 压缩后占用大幅下降;旧占用显示清零,待下一次真实 usage 再更新。
            // 是"未知"不是"0%":摘要占多少只有下一次请求才知道。
            // 注意:累计 in/out 不归零——压缩没换会话,已经花掉的账不该消失。
            stats.last_prompt = None;
        }
        SessionEvent::SessionInfo { id, .. } => {
            // 会话创建事件(启动即到达):记下当前会话 id 供状态栏展示
            *session_id = id;
        }
        SessionEvent::HistoryLoaded { count, messages } => {
            // /resume:当前显示的内容已不在上下文中,整体替换为恢复的历史会话
            items.clear();
            // 换了会话:状态栏的 ctx 占用与累计 in/out 都是**上一个会话**的数字,
            // 留着会让人以为恢复出来的历史占据/花费了这些(真实值等下一次 usage 到账)
            *stats = TokenStats::default();
            // 但"默认的空"在这里是错的:恢复出来的历史实打实占着上下文,只是占多少
            // 得等下一轮 usage 才算得出——显式标"未知",徽标画 `ctx ?` 而不是 0%。
            stats.last_prompt = None;
            items.push(MsgItem::Notice(format!(
                "━━━ 已恢复历史会话({count} 条消息),内容如下 ━━━"
            )));
            // 记录 tool_call_id → 工具名,便于给 tool 结果消息命名
            let mut tool_names: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            // assistant 声明的调用 → 卡片下标,等 tool 结果消息回填
            let mut pending: std::collections::HashMap<String, usize> =
                std::collections::HashMap::new();
            for m in &messages {
                push_history_item(items, m, &mut tool_names, &mut pending);
            }
        }
        SessionEvent::TurnFinished { text, truncated } => {
            *busy = false;
            *busy_kind = BusyKind::Work;
            // 回合结束:丢弃未结算的估算(端点无 usage 时该轮无真实记账)
            stats.round_est = 0;
            if !text.is_empty() {
                match items.last_mut() {
                    Some(MsgItem::AssistantStream(s)) => {
                        s.push_str(if truncated { "\n[⚠ 未完成]" } else { "" });
                    }
                    _ => {
                        items.push(MsgItem::Assistant(if truncated {
                            format!("[⚠ 未完成] {text}")
                        } else {
                            text
                        }));
                    }
                }
            }
            *permission = None;
        }
    }
}

/// 粗略 token 估算(无本地 tokenizer,仅供实时显示,量级正确即可):
/// 中文等非 ASCII 字符约 1 token/字,ASCII 文本约 4 字符/token。
fn est_tokens(s: &str) -> u64 {
    let mut ascii = 0u64;
    let mut other = 0u64;
    for c in s.chars() {
        if c.is_ascii() {
            ascii += 1;
        } else {
            other += 1;
        }
    }
    other + ascii.div_ceil(4)
}

/// token 数缩写显示:1234 → "1.2k",2_300_000 → "2.3M"
fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// 上下文占用徽标:10 格占用条 + 百分比,窗口已知就常驻显示;
/// 占用 90% 及以上变红、70% 及以上变黄、其余灰。
/// **占用还没测到时**(`None`,如刚 /resume 或 /compact 完)画 `ctx ?`,不编成 0%——
/// resume 出来的历史明明占着上下文,画成空条会让人以为还能再塞很多。
/// 窗口未知(模型不在内置表里、配置里也没写死)时不编百分比:只报真实绝对量
/// `ctx ~21.4k`,也不参与变红与压缩提示——猜错的窗口比没有窗口更误事。
fn ctx_usage_badge(used: Option<u64>, window: Option<usize>) -> Option<(String, Color)> {
    let window = match window {
        Some(w) => w,
        None => {
            let txt = match used {
                Some(n) if n > 0 => format!("ctx ~{} 窗口未知", fmt_tokens(n)),
                _ => "ctx ? 窗口未知".to_string(),
            };
            return Some((txt, Color::DarkGray));
        }
    };
    if window == 0 {
        return None;
    }
    // 占用还没测到(刚 /resume 或 /compact 完、或还没发过消息):不编 0%——那会让人
    // 以为上下文很空,而 resume 出来的历史其实占着;如实说"?"。真实值在下一轮回复后到账。
    let Some(used) = used else {
        return Some(("ctx ?".to_string(), Color::DarkGray));
    };
    let raw = if used == 0 {
        0
    } else {
        ((used as u128) * 100 / (window as u128)) as usize
    };
    let pct = raw.min(100);
    let filled = ((pct as f64) / 10.0).round().clamp(0.0, 10.0) as usize;
    let bar: String = "█".repeat(filled) + &"░".repeat(10 - filled);
    let color = if pct >= 90 {
        Color::Red
    } else if pct >= 70 {
        Color::Yellow
    } else {
        Color::DarkGray
    };
    Some((format!("ctx {bar} {pct}%"), color))
}

// ---- 工作动画(flowing-bar 光条家族)----
// 主循环 ~120ms/帧:anim 帧号 = elapsed/120ms,各动画点同相;空闲不显示即零开销。

const BAR_LEVELS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// 流动光条:返回 n 段高度序列(峰值 █、谷值 ▁),相位递增时光峰左移。
fn wave_row(phase: usize, n: usize) -> String {
    let n = n.max(2);
    let half = n / 2;
    let mut out = String::with_capacity(n);
    for k in 0..n {
        let t = (k + phase) % n;
        let pos = if t <= half { t } else { n - t }; // 三角波:0..=half 再回落
        let idx = (pos * 7 / half.max(1)).min(7);
        out.push(BAR_LEVELS[idx]);
    }
    out
}

/// 单格脉动(流式输出行尾光标 / 工具卡片执行中):音量式升-落循环
fn pulse_char(slow_frame: usize) -> char {
    const P: [char; 8] = ['▁', '▃', '▅', '▇', '█', '▇', '▅', '▃'];
    P[slow_frame % P.len()]
}

/// 执行中卡片实时输出区最多显示多少个屏幕行(tail 窗口)
const LIVE_ROWS: usize = 10;
/// 实时输出累积上限(字节):超出丢头部只留尾(UI 只关心最新)
const LIVE_CAP: usize = 128 * 1024;

/// 工具卡片的成品输出留存上限:卡片只展示前 500 字符 / 4 行,存整份(命令可能吐
/// 几 MB)只会白占内存到会话结束。保头 + 标记,够渲染就够。
const TOOL_OUT_CAP: usize = 8 * 1024;

/// 工具输出入库前的封顶(保头,超限处补一行说明)
fn cap_tool_output(s: String) -> String {
    if s.len() <= TOOL_OUT_CAP {
        return s;
    }
    let idx = s
        .char_indices()
        .find_map(|(i, _)| (i >= TOOL_OUT_CAP).then_some(i))
        .unwrap_or(s.len());
    format!(
        "{}\n…(输出过长,后面 {} 字符未留存;完整内容已交给模型/历史文件)",
        &s[..idx],
        s.chars().count() - s[..idx].chars().count()
    )
}

/// 实时输出缓冲封顶:超限丢头保尾(按 char 边界切)
fn cap_live_tail(s: &mut String) {
    if s.len() > LIVE_CAP {
        let cut = s.len() - LIVE_CAP / 2;
        // 找到字节位置 >= cut 的第一个字符边界
        let idx = s
            .char_indices()
            .find_map(|(i, _)| (i >= cut).then_some(i))
            .unwrap_or(s.len());
        s.drain(..idx);
    }
}

/// 取文本末尾最多 n 个逻辑行(不足则全取;末行未换行也算一行)
fn last_n_lines(s: &str, n: usize) -> String {
    if s.is_empty() {
        return String::new();
    }
    let mut lines: Vec<&str> = s.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    let take = lines.len().min(n);
    lines[lines.len() - take..].join("\n")
}

/// 静默预算毫秒 → 简短人读:90s / 240s / 10min
fn fmt_ms_budget(ms: u64) -> String {
    let secs = ms / 1000;
    if secs >= 60 && secs % 60 == 0 {
        format!("{}min", secs / 60)
    } else {
        format!("{secs}s")
    }
}

/// 工具卡片的"调用内容"摘要:命令类剥掉 JSON 外壳、直接给 command 原文
/// (cwd 有指定时顺带标注),其余工具原样展示参数;无参/空参返回空串不显示
fn tool_args_summary(name: &str, args: &str) -> String {
    let args = args.trim();
    if args.is_empty() {
        return String::new();
    }
    if name == "run_shell_command" {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(args) {
            if let Some(cmd) = v.get("command").and_then(|c| c.as_str()) {
                if !cmd.is_empty() {
                    let mut s = format!("$ {cmd}");
                    if let Some(c) = v.get("cwd").and_then(|c| c.as_str()) {
                        if !c.is_empty() {
                            s.push_str(&format!("  (cwd: {c})"));
                        }
                    }
                    return s;
                }
            }
        }
        return args.to_string();
    }
    // 其它工具:空参数对象({} 或显式 null)不显示
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(args) {
        let empty = match &v {
            serde_json::Value::Object(o) => o.is_empty(),
            serde_json::Value::Null => true,
            _ => false,
        };
        if empty {
            return String::new();
        }
    }
    args.to_string()
}

/// 收纳推进条(压缩/归档类动作):整块向细条收拢再弹开,表达"正在收纳/压缩"
fn shrink_char(frame: usize) -> char {
    const S: [char; 14] = [
        '█', '▉', '▊', '▋', '▌', '▍', '▎', '▏', '▎', '▍', '▌', '▋', '▊', '▉',
    ];
    S[frame % S.len()]
}

/// 忙碌的场景:回合执行 / 上下文压缩(决定忙行文案与动画样式)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BusyKind {
    Work,
    Compacting,
}

// ---- 输入补全(slash 命令 / @ 文件引用)----

/// 补全候选:label = 菜单显示文本,insert = 采纳时的替换文本(如引号包裹),hint = 右侧说明
struct CompletionItem {
    label: String,
    insert: String,
    hint: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompletionKind {
    Slash,
    At,
}

/// 输入补全状态(菜单打开)
struct Completion {
    kind: CompletionKind,
    /// 已输入前缀(token 内 / 或 @ 之后的原文)
    typed: String,
    /// 当前词起点(含 / 或 @)的字节位置,采纳时替换 [word_start, cursor)
    word_start: usize,
    items: Vec<CompletionItem>,
    selected: usize,
}

const SLASH_BUILTINS: &[(&str, &str)] = &[
    ("/help", "显示帮助"),
    ("/skills", "已安装技能列表"),
    ("/config", "打开配置向导"),
    ("/cfg", "按需改配置(选一项改一项,可直接提交/验证后提交)"),
    ("/undo", "undo 快照/回滚(/undo <序号>)"),
    (
        "/resume",
        "历史会话管理窗口(批量删/备注/恢复); /resume <片段> 直接恢复",
    ),
    ("/clear", "清空会话(确认后不可恢复)"),
    ("/compact", "压缩上下文:旧对话 → 摘要,释放空间"),
    ("/update", "检查并更新到 GitHub 最新版本"),
    (
        "/persona",
        "人格设置:列表/切换(/persona <名字>, /persona 无 关闭)",
    ),
    ("/quit", "退出本次会话(显示统计);/exit 同效"),
];

/// 光标处的补全词探测。返回 (种类, 词起点字节, 已输入前缀)。
/// @ 在任意位置触发(与 refs 边界一致);/ 只在整段开头第一个词触发。
fn completion_detect(input: &str, cursor: usize) -> Option<(CompletionKind, usize, String)> {
    if cursor == 0 {
        return None;
    }
    let left = &input[..cursor];
    // 限定在光标所在行内分析(换行是天然分隔)
    let line_base = left.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line = &left[line_base..];
    // 引号模式优先:行内最近一个 @"且未闭合 —— 引号内空格不算分隔
    if let Some(q) = line.rfind("@\"") {
        let after = &line[q + 2..];
        if !after.is_empty() && !after.contains('"') {
            return Some((CompletionKind::At, line_base + q, after.to_string()));
        }
    }
    let is_sep = |c: char| {
        c.is_whitespace()
            || matches!(
                c,
                ',' | '。' | ';' | '\n' | '\r' | '」' | ')' | ']' | '}' | '：' | ':'
            )
    };
    let mut word_start = line_base;
    for (i, c) in line.char_indices() {
        if is_sep(c) {
            word_start = line_base + i + c.len_utf8();
        }
    }
    let word = &input[word_start..];
    if let Some(rest) = word.strip_prefix('@') {
        // 普通模式:允许空前缀(刚输入 @)列出当前层全部候选
        return Some((CompletionKind::At, word_start, rest.to_string()));
    }
    if let Some(rest) = word.strip_prefix('/') {
        // slash 命令只在整段开头的第一个词触发
        if left[..word_start].trim().is_empty() {
            return Some((CompletionKind::Slash, word_start, rest.to_string()));
        }
    }
    None
}

/// 前缀匹配(大小写不敏感,供补全过滤;采纳文本保持磁盘原名)
fn prefix_ci(name: &str, typed: &str) -> bool {
    if typed.is_empty() {
        return true;
    }
    let n = name.chars().count();
    let t = typed.chars().count();
    if t > n {
        return false;
    }
    name.chars()
        .zip(typed.chars())
        .all(|(a, b)| a.eq_ignore_ascii_case(&b))
}

/// slash 候选:内置命令 + 已安装技能(前缀过滤)
fn slash_candidates(cwd: &Path, typed: &str) -> Vec<CompletionItem> {
    let mut out = Vec::new();
    for (cmd, desc) in SLASH_BUILTINS {
        if prefix_ci(&cmd[1..], typed) {
            out.push(CompletionItem {
                label: cmd.to_string(),
                insert: cmd.to_string(),
                hint: desc.to_string(),
            });
        }
    }
    let set = znaide_core::skills::scan(cwd);
    for s in set.skills {
        if !prefix_ci(&s.name, typed) {
            continue;
        }
        let flag = if s.disable_model_invocation {
            " [仅手动]"
        } else {
            ""
        };
        let hint = format!("技能 · {}{}", s.description, flag);
        out.push(CompletionItem {
            label: format!("/{}", s.name),
            insert: format!("/{}", s.name),
            hint,
        });
    }
    out
}

/// 名称含空白或引用边界字符时用 @"..." 形式(与 refs 引号解析一致)
fn quote_at_path(name: &str) -> String {
    let needs = name.chars().any(|c| {
        c.is_whitespace() || matches!(c, ',' | '。' | ';' | '」' | ')' | ']' | '}' | '：' | ':')
    });
    if needs {
        format!("\"{name}\"")
    } else {
        name.to_string()
    }
}

/// @ 补全候选:按已输入路径逐层列目录(对齐 refs 的 ~/ 展开与相对 cwd 语义)
fn at_candidates(cwd: &Path, typed: &str) -> Vec<CompletionItem> {
    // 拆目录部分(含尾 /)与文件名前缀
    let (dir_part, base) = match typed.rfind('/') {
        Some(i) => (&typed[..=i], &typed[i + 1..]),
        None => ("", typed),
    };
    let dir = if let Some(rest) = dir_part.strip_prefix("~/") {
        match dirs::home_dir() {
            Some(h) => h.join(rest),
            None => return Vec::new(),
        }
    } else {
        let p = PathBuf::from(dir_part);
        if p.is_absolute() {
            p
        } else if dir_part.is_empty() {
            cwd.to_path_buf()
        } else {
            cwd.join(p)
        }
    };
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<CompletionItem> = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.contains('"') {
            continue; // 引号内无法表达,不提示避免"能选不能用"
        }
        if name.starts_with('.') && !base.starts_with('.') {
            continue; // 隐藏条目默认不列
        }
        if !prefix_ci(&name, base) {
            continue;
        }
        // 完整相对路径(自补全起点,即 @ 之后已输入目录 + 条目名)
        let mut full = if dir_part.is_empty() {
            name
        } else {
            format!("{dir_part}{name}")
        };
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let hint = if is_dir { "目录" } else { "文件" };
        if is_dir {
            full.push('/');
        }
        out.push(CompletionItem {
            label: full.clone(),
            insert: quote_at_path(&full),
            hint: hint.into(),
        });
    }
    out.sort_by(|a, b| a.label.cmp(&b.label));
    out.truncate(200);
    out
}

/// 采纳高亮候选:替换 [word_start, cursor) 为 insert。
/// At 类候选的 insert 不含 @(含引号包裹的完整路径),这里统一补上前缀。
/// 采纳后自动补一个空格便于继续输入:目录项(以 / 结尾,还要继续深入)不补;
/// 光标后已是空白时不补,避免双空格。
fn accept_completion(input: &mut String, cursor: &mut usize, cm: &Completion) {
    let n = cm.items.len();
    if n == 0 {
        return;
    }
    let it = &cm.items[cm.selected.min(n - 1)];
    let mut full = if cm.kind == CompletionKind::At {
        format!("@{}", it.insert)
    } else {
        it.insert.clone()
    };
    // 目录以 / 结尾(label 未被引号包裹),还要继续深入 → 不补空格
    let is_dir = it.label.ends_with('/');
    let next_ws = input[*cursor..]
        .chars()
        .next()
        .map(|c| c.is_whitespace())
        .unwrap_or(false);
    if !is_dir && !next_ws {
        full.push(' ');
    }
    input.replace_range(cm.word_start..*cursor, &full);
    *cursor = cm.word_start + full.len();
}

/// ghost 尾巴:高亮候选相对已输入前缀的剩余显示文本(供光标行末尾淡色预览)
fn ghost_tail(cm: &Completion) -> Option<String> {
    if cm.items.is_empty() {
        return None;
    }
    let it = &cm.items[cm.selected.min(cm.items.len() - 1)];
    let body: &str = match cm.kind {
        CompletionKind::Slash => it.label.strip_prefix('/').unwrap_or(&it.label),
        CompletionKind::At => &it.label,
    };
    let tail = body.strip_prefix(&cm.typed)?;
    if tail.is_empty() {
        None
    } else {
        Some(tail.to_string())
    }
}

/// 每帧刷新补全状态:无触发/无候选则关闭;输入前缀与光标位置未变时保留选中项,
/// 避免每个 120ms 帧都做磁盘扫描。
fn refresh_completion(
    input: &str,
    cursor: usize,
    cwd: &Path,
    enabled: bool,
    cm: &mut Option<Completion>,
) {
    let det = if enabled {
        completion_detect(input, cursor)
    } else {
        None
    };
    let Some((kind, word_start, typed)) = det else {
        *cm = None;
        return;
    };
    if let Some(c) = cm {
        if c.kind == kind && c.word_start == word_start && c.typed == typed {
            return; // 无变化:保留选中
        }
    }
    let items = match kind {
        CompletionKind::Slash => slash_candidates(cwd, &typed),
        CompletionKind::At => at_candidates(cwd, &typed),
    };
    if items.is_empty() {
        *cm = None;
        return;
    }
    *cm = Some(Completion {
        kind,
        typed,
        word_start,
        items,
        selected: 0,
    });
}

/// 补全菜单:浮在输入框正上方的候选列表(高亮当前选中,右侧为说明)
fn draw_completion_menu(f: &mut ratatui::Frame<'_>, input_area: Rect, cm: &Completion) {
    let n = cm.items.len();
    if n == 0 {
        return;
    }
    let width = input_area.width.saturating_sub(2).max(20);
    let max_rows = 8u16;
    let rows = (n as u16).min(max_rows);
    let height = rows + 2; // + 边框上下
    let x = input_area.x + 1;
    let y = input_area.y.saturating_sub(height).max(1);
    let menu = Rect {
        x,
        y,
        width,
        height,
    };
    f.render_widget(Clear, menu);
    let block = Block::default()
        .borders(ratatui::widgets::Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(menu);
    f.render_widget(block, menu);
    // 可视窗口:让选中项保持在视野内
    let view = rows as usize;
    let start = cm
        .selected
        .saturating_sub(view.saturating_sub(1))
        .min(n.saturating_sub(view));
    let avail = width.saturating_sub(4) as usize;
    let mut ls: Vec<Line<'static>> = Vec::new();
    for (idx, it) in cm.items.iter().enumerate().skip(start).take(view) {
        let selected = idx == cm.selected;
        let style = if selected {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        // label + hint 右对齐(截断保护)
        let hint_w = it.hint.chars().count().min(28);
        let label_max = avail.saturating_sub(hint_w + 1).max(4);
        let label = truncate(&it.label, label_max);
        let pad = avail.saturating_sub(label.chars().count() + hint_w);
        let mut spans = vec![Span::styled(format!(" {}", label), style)];
        if it.hint.is_empty() {
            spans.push(Span::styled(" ".repeat(pad), style));
        } else {
            spans.push(Span::styled(
                format!("{}{}", " ".repeat(pad), truncate(&it.hint, hint_w)),
                style,
            ));
        }
        ls.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(ls), inner);
}

/// 输入区内容行数上限:超过后输入框不再增高,内部滚动跟随光标
const MAX_INPUT_ROWS: usize = 8;

/// 工作中输入队列:回合执行中按 Enter 入队,消息交给会话引擎在**下一个轮边界**
/// 插进对话(它自己会继续往下干,不会被这条打断);若这一回合已没有后续轮次
/// (模型给出终结答复),队列里剩下的在回合结束时作为新回合发出。
///
/// 存储与 `Session` 共享同一个投递箱(`SharedInbox`),因此"队列 N"只有一个来源:
/// 引擎取走一条,UI 这边的计数自动减少,不会出现两边各记一份而对不上。
#[derive(Default)]
struct InputQueue {
    items: SharedInbox,
}

impl InputQueue {
    fn new(items: SharedInbox) -> Self {
        Self { items }
    }

    /// 入队一条,返回给用户看的提示(预览压成一行并按字符截断,不切坏中文)。
    fn push(&self, raw: &str) -> String {
        if let Ok(mut q) = self.items.lock() {
            q.push_back(raw.to_string());
        }
        let flat = raw.replace('\n', " ");
        format!(
            "⏳ 已排队(下一个轮边界发出):{}",
            znaide_core::util::truncate_chars(&flat, 40, "…")
        )
    }

    /// 回合结束(空闲)且没有模态弹窗时取出下一条,当作新回合发出。
    /// 忙的时候不取——正在跑的回合由引擎自己在轮边界插话。
    fn pop_ready(&mut self, idle: bool, modal: bool) -> Option<String> {
        if idle && !modal {
            self.items.lock().ok()?.pop_front()
        } else {
            None
        }
    }

    fn len(&self) -> usize {
        self.items.lock().map(|q| q.len()).unwrap_or(0)
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 输入框前缀:首行 "❯[label] ",续行等宽缩进。渲染与高度计算共用,保证折行口径一致。
fn input_prefix(label: &str) -> (String, String) {
    let first = format!("❯{label} ");
    (first.clone(), " ".repeat(first.chars().count()))
}

/// 输入前过滤:剥掉终端控制字节与转义序列(CSI/OSC/SGR 鼠标残片等),保留换行/制表。
/// 鼠标字节流若被外部进程撕开,残片会被当按键文本塞进来,这里兜个底。
fn sanitize_typed_text(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\x1b' {
            // CSI: ESC [ … 直到 ascii 字母 / ~ 结束
            if i + 1 < chars.len() && chars[i + 1] == '[' {
                i += 2;
                while i < chars.len() {
                    let e = chars[i];
                    if e.is_ascii_alphabetic() || e == '~' {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                continue;
            }
            // 其它 ESC 序列:丢弃 ESC 及其后直到下一个控制符/ESC 的可打印区
            i += 1;
            while i < chars.len() && !chars[i].is_control() && chars[i] != '\x1b' {
                i += 1;
            }
            continue;
        }
        if c.is_control() && c != '\n' && c != '\t' {
            i += 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// 输入内容折行后的物理行数(空输入 = 1;与 render_input 用同一折行函数)。
/// body_w 为扣除前缀后的可用宽度。
fn input_phys_lines(input: &str, body_w: usize) -> usize {
    let bw: u16 = body_w.clamp(1, u16::MAX as usize) as u16;
    let mut n = 0usize;
    for line in input.split('\n') {
        let segs = crate::md::wrap_plain(line, bw);
        n += segs.len().max(1);
    }
    n.max(1)
}

/// 渲染输入区(多行)。把逻辑行按宽度折成物理行,窗口以光标行为中心,
/// 光标所在位置反白显示。返回内容已限制在 max_rows 行内。
fn render_input(
    input: &str,
    cursor: usize,
    label: &str,
    style: Style,
    max_rows: usize,
    width: u16,
    ghost: Option<&str>,
) -> Paragraph<'static> {
    let cursor = cursor.min(input.len());
    let prompt_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let cursor_style = Style::default()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let ghost_style = Style::default().fg(Color::DarkGray);

    // 前缀宽度(首行 "❯ …label ");后续物理行用等宽缩进保持对齐
    let (prefix_first, prefix_cont) = input_prefix(label);
    let prefix_w = prefix_first.chars().count() as u16;
    let body_w = width.saturating_sub(prefix_w).max(4);

    // 光标所在逻辑行与列
    let before = &input[..cursor];
    let cursor_line = before.bytes().filter(|b| *b == b'\n').count();
    let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let cursor_col_chars = before[line_start..].chars().count();

    // 先确定每行文本
    let all_lines: Vec<&str> = input.split('\n').collect();
    let cursor_text = all_lines.get(cursor_line).copied().unwrap_or_default();

    // 精确定位光标:光标所在物理行与列
    // 先求光标行的物理分块(按 wrap_plain 逻辑与上面一致)
    let cursor_segs = crate::md::wrap_plain(cursor_text, body_w.max(1));
    let mut seg_ends: Vec<usize> = Vec::new(); // 每块结束时的字符数
    let mut acc = 0usize;
    for s in &cursor_segs {
        acc += s.chars().count();
        seg_ends.push(acc);
    }
    let mut cursor_phys_line_in = cursor_segs.len().saturating_sub(1);
    let mut cursor_col_in_seg = 0usize;
    for (si, end) in seg_ends.iter().enumerate() {
        if cursor_col_chars <= *end {
            cursor_phys_line_in = si;
            let prev = if si == 0 { 0 } else { seg_ends[si - 1] };
            cursor_col_in_seg = cursor_col_chars.saturating_sub(prev);
            break;
        }
    }
    // 全局物理行号 = 光标行之前所有行的物理行数 + 光标行内行号
    let mut phys_before = 0usize;
    for li in 0..cursor_line {
        let segs = crate::md::wrap_plain(
            all_lines.get(li).copied().unwrap_or_default(),
            body_w.max(1),
        );
        phys_before += segs.len().max(1);
    }
    let cursor_phys_global = phys_before + cursor_phys_line_in;

    // 重排 phys(确保光标行物理分块一致)
    let mut phys2: Vec<(usize, String, bool)> = Vec::new();
    for (li, text) in all_lines.iter().enumerate() {
        let segs = crate::md::wrap_plain(text, body_w.max(1));
        if segs.is_empty() {
            // 空逻辑行(含整框为空时)也占一行:❯ 与光标必须始终可见
            phys2.push((li, String::new(), false));
        } else {
            for seg in segs {
                phys2.push((li, seg, false));
            }
        }
    }
    // 为光标物理行画光标列
    let total_phys = phys2.len();
    let win_h = max_rows.min(total_phys.max(1));
    // 窗口:光标行尽量处于窗口中央或偏下
    let want_end = cursor_phys_global + 1;
    let win_start = want_end.saturating_sub(win_h);
    let win_start = win_start.min(total_phys.saturating_sub(win_h));
    let win_end = (win_start + win_h).min(total_phys);

    let mut lines: Vec<Line<'static>> = Vec::new();
    for (pi, (li, seg, _isc)) in phys2.iter().enumerate() {
        if pi < win_start || pi >= win_end {
            continue;
        }
        let prefix = if *li == 0 {
            &prefix_first
        } else {
            &prefix_cont
        };
        let mut spans: Vec<Span<'static>> = Vec::new();
        spans.push(Span::styled(prefix.clone(), prompt_style));
        if pi == cursor_phys_global {
            // 光标行:在 cursor_col_in_seg 处反白
            let chars: Vec<char> = seg.chars().collect();
            let mut emitted = false;
            for (ci, ch) in chars.iter().enumerate() {
                if ci == cursor_col_in_seg && !emitted {
                    spans.push(Span::styled(ch.to_string(), cursor_style));
                    emitted = true;
                } else {
                    spans.push(Span::styled(ch.to_string(), Style::default()));
                }
            }
            if !emitted {
                if chars.is_empty() {
                    spans.push(Span::styled(" ", cursor_style));
                } else {
                    spans.push(Span::styled("▍", cursor_style));
                }
            }
            // ghost 预览:光标在输入末尾时,淡色显示高亮候选的剩余文本
            if let Some(g) = ghost {
                if !g.is_empty() {
                    spans.push(Span::styled(g.to_string(), ghost_style));
                }
            }
        } else {
            spans.push(Span::styled(seg.clone(), Style::default()));
        }
        lines.push(Line::from(spans));
    }
    // 不满窗口时补空行
    while lines.len() < win_h {
        lines.push(Line::from(""));
    }
    Paragraph::new(lines).style(style)
}

fn build_content_lines(
    items: &[MsgItem],
    busy: bool,
    busy_kind: BusyKind,
    width: u16,
    anim_frame: usize,
) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    for item in items {
        match item {
            MsgItem::Logo => {
                // 启动横幅:几何大字,逐行青→品红渐变
                let lines = logo_lines();
                let total = lines.len().saturating_sub(1).max(1);
                for (r, line) in lines.into_iter().enumerate() {
                    let t = r as f32 / total as f32;
                    let r8 = (255.0 * t) as u8;
                    let g8 = (255.0 * (1.0 - t)) as u8;
                    let b8 = 255u8;
                    out.push(Line::from(Span::styled(
                        line,
                        Style::default()
                            .fg(Color::Rgb(r8, g8, b8))
                            .add_modifier(Modifier::BOLD),
                    )));
                }
                out.push(Line::from(""));
            }
            MsgItem::User(u) => {
                // 用户消息:md 渲染 + 前缀
                let segs = crate::md::render(u, width.saturating_sub(4));
                for (i, seg) in segs.iter().enumerate() {
                    let mut spans = Vec::new();
                    if i == 0 {
                        spans.push(Span::styled(
                            "❯ 你: ",
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        ));
                    } else {
                        spans.push(Span::styled(
                            "      ",
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        ));
                    }
                    spans.extend(seg.spans.iter().cloned());
                    out.push(Line::from(spans));
                }
                out.push(Line::from(""));
            }
            MsgItem::Assistant(a) | MsgItem::AssistantStream(a) => {
                let is_streaming = matches!(item, MsgItem::AssistantStream(_));
                // 流式输出行尾:脉动光标(每 2 帧步进一次,慢半拍更从容)
                let text = if is_streaming {
                    format!("{a}{}", pulse_char(anim_frame / 2))
                } else {
                    a.clone()
                };
                // 助手消息:完整 md 渲染
                let mut md_lines = crate::md::render(&text, width);
                if md_lines.is_empty() {
                    md_lines.push(Line::from(""));
                }
                for l in md_lines {
                    out.push(l);
                }
                out.push(Line::from(""));
            }
            MsgItem::Reasoning(r) => {
                let dim = Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC);
                let segs = crate::md::wrap_plain(r, width.saturating_sub(2));
                for (i, seg) in segs.iter().enumerate() {
                    let prefix = if i == 0 { "… " } else { "" };
                    out.push(Line::from(Span::styled(format!("{prefix}{seg}"), dim)));
                }
                out.push(Line::from(""));
            }
            MsgItem::Tool {
                name,
                args,
                ok,
                output,
                started,
                done_secs,
                idle_ms,
                silent,
                live,
            } => {
                // 执行中:脉动光标;完成:✓ / ✗
                let icon = match ok {
                    None => pulse_char(anim_frame / 2).to_string(),
                    Some(true) => "✓".to_string(),
                    Some(false) => "✗".to_string(),
                };
                // 执行中的命令:静默预警接近超时时黄 → 红,平时蓝
                let color = match (*ok, *silent) {
                    (None, 2) => Color::Red,
                    (None, 1) => Color::Yellow,
                    (None, _) => Color::Blue,
                    (Some(true), _) => Color::Green,
                    (Some(false), _) => Color::Red,
                };
                // 动态计时:执行中 = 已运行(每秒跳)+ AI 预算;完成后 =
                // 定格的总用时(不再随帧增长);历史回放卡片(started=None)不显示
                let mut head = format!("  {icon} {name}");
                if ok.is_none() {
                    if let Some(st) = started {
                        let elapsed_ms = st.elapsed().as_millis() as u64;
                        head.push_str(&format!(" · {}s", elapsed_ms / 1000));
                        if let Some(ms) = idle_ms {
                            head.push_str(&format!(" · 预算 {}", fmt_ms_budget(*ms)));
                            // 已超预算:进入宽限期(预算 20%、最多 60s),
                            // 明示剩余宽限,让用户知道命令随时可能被终止
                            if elapsed_ms > *ms {
                                let grace = znaide_core::tools::shell::overrun_grace_ms(*ms);
                                let left = (*ms + grace).saturating_sub(elapsed_ms);
                                head.push_str(&format!(" · 已超预算,宽限剩 {}s", left / 1000));
                            }
                        }
                    }
                } else if let Some(secs) = done_secs {
                    head.push_str(&format!(" · {secs}s"));
                }
                out.push(Line::from(Span::styled(
                    head,
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                )));
                // 调用内容(浅灰小字):命令显示原文,其它工具显示参数;
                // 执行中与完成后都保留,最多折 2 行
                let summary = tool_args_summary(name, args);
                if !summary.is_empty() {
                    for (i, line) in crate::md::wrap_plain(&summary, width.saturating_sub(8))
                        .into_iter()
                        .take(2)
                        .enumerate()
                    {
                        let indent = if i == 0 { "      " } else { "        " };
                        out.push(Line::from(Span::styled(
                            format!("{indent}{line}"),
                            Style::default().fg(Color::DarkGray),
                        )));
                    }
                }
                // 执行中的实时输出区(tail -f 式):只显示最新几行,超预算等
                // 状态在标题行已提示;完成后回落,输出改由下方 output 呈现
                if ok.is_none() && !live.is_empty() {
                    out.push(Line::from(Span::styled(
                        "      ─────── 实时输出 ───────",
                        Style::default().fg(Color::DarkGray),
                    )));
                    // 取 live 尾部若干逻辑行作输入,wrap 后再取最后 LIVE_ROWS 个
                    // 屏幕行——保证"最新"在视野里
                    let base = Style::default().fg(Color::DarkGray);
                    let tail_src = last_n_lines(live, 40);
                    let rendered =
                        crate::ansi::render_lines(&tail_src, width.saturating_sub(8), base);
                    let skip = rendered.len().saturating_sub(LIVE_ROWS);
                    for line in rendered.into_iter().skip(skip) {
                        let mut spans = vec![Span::raw("      ")];
                        spans.extend(line.spans);
                        out.push(Line::from(spans));
                    }
                }
                if !output.is_empty() {
                    let out_text = truncate(output, 500);
                    // 工具/命令输出可能带 ANSI 颜色:解析成样式保留观感,
                    // 同时剥掉其余控制字节(否则真实 ESC 会被终端执行、污染整屏)
                    let base = Style::default().fg(Color::DarkGray);
                    for (i, line) in
                        crate::ansi::render_lines(&out_text, width.saturating_sub(8), base)
                            .into_iter()
                            .enumerate()
                    {
                        if i >= 4 {
                            break;
                        }
                        let mut spans = vec![Span::raw("      ")];
                        spans.extend(line.spans);
                        out.push(Line::from(spans));
                    }
                }
                out.push(Line::from(""));
            }
            MsgItem::Notice(n) => {
                for (i, line) in crate::md::wrap_plain(n, width.saturating_sub(2))
                    .iter()
                    .enumerate()
                {
                    let prefix = if i == 0 { "ℹ " } else { "   " };
                    out.push(Line::from(Span::styled(
                        format!("{prefix}{line}"),
                        Style::default().fg(Color::Yellow),
                    )));
                }
                out.push(Line::from(""));
            }
            MsgItem::CommandOutput(t) => {
                for line in crate::md::wrap_plain(t, width.saturating_sub(2)) {
                    out.push(Line::from(Span::styled(
                        line,
                        Style::default().fg(Color::Magenta),
                    )));
                }
                out.push(Line::from(""));
            }
        }
    }
    if busy {
        // 消息区底部的动态忙行:执行 = 流动光条 + Esc 中断;压缩 = 收纳推进条 + 不可中断
        let (prefix, text, color) = match busy_kind {
            BusyKind::Work => (
                wave_row(anim_frame, 4),
                "思考执行中…(Esc 中断)",
                Color::Blue,
            ),
            BusyKind::Compacting => (
                shrink_char(anim_frame / 2).to_string(),
                "压缩中(旧消息 → 摘要),不可中断,约数秒~数十秒",
                Color::Cyan,
            ),
        };
        out.push(Line::from(Span::styled(
            format!("{prefix} {text}"),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )));
    }
    out
}

/// 消息区右侧滚动条:内容超过可视高度时,在右边框列上画一段拇指。
/// scroll_back = 从底部算起的偏移行数(0 = 在底部)。thumb 高度按内容比例缩放。
fn draw_msg_scrollbar(
    f: &mut ratatui::Frame<'_>,
    area: Rect,
    total: usize,
    view_h: usize,
    scroll_back: usize,
) {
    if total <= view_h || view_h == 0 {
        return; // 内容不满一屏:无需滚动条
    }
    let range = total - view_h; // 可滚动行数 = scroll_back 的取值范围
    let area_h = view_h;
    // 拇指高度:与"可视/总内容"成比例,至少 1 行、至多整块
    let thumb = ((area_h * area_h) / total).clamp(1, area_h);
    let travel = area_h - thumb;
    let sb = scroll_back.min(range);
    // 拇指顶部在可视区内的行号:scroll_back=0(看最新)→ 贴底;=range(看最旧)→ 贴顶
    let pos = if range > 0 {
        ((range - sb) * travel) / range
    } else {
        0
    };

    let col = area.right().saturating_sub(1); // 右边框列
    let style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::BOLD);
    let buf = f.buffer_mut();
    for r in 0..area_h {
        let row = area.y + 1 + r as u16;
        if r >= pos && r < pos + thumb {
            let cell = &mut buf[(col, row)];
            cell.set_symbol("█");
            cell.set_style(style);
        }
        // 其余行保留段落右边框的 '│'
    }
}

/// 处理鼠标:消息区内滚轮滚动(↑/↓ 等价步进);按住右侧滚动条拖动或点击跳转。
#[allow(clippy::too_many_arguments)]
fn handle_mouse_scroll(
    m: crossterm::event::MouseEvent,
    area: Rect,
    range: usize,
    scroll_back: &mut usize,
    at_bottom: &mut bool,
    dragging: &mut bool,
) {
    use crossterm::event::{MouseButton, MouseEventKind};
    if scroll_range_is_irrelevant(range, area) {
        return;
    }
    // 只响应消息区(去上下边框的行内区域)
    let in_area = m.column < area.right()
        && m.row >= area.y + 1
        && m.row <= area.y.saturating_add(area.height).saturating_sub(2);
    if !in_area {
        return;
    }
    match m.kind {
        MouseEventKind::ScrollUp => {
            *scroll_back = scroll_back.saturating_add(3);
            *at_bottom = false;
        }
        MouseEventKind::ScrollDown => {
            *scroll_back = scroll_back.saturating_sub(3);
            if *scroll_back == 0 {
                *at_bottom = true;
            }
        }
        MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Drag(MouseButton::Left)
            if m.column == area.right().saturating_sub(1) =>
        {
            // 在滚动条(右边框列)上按下/拖动:持续跟随
            *dragging = true;
            jump_scroll_to(m.row, area, range, scroll_back, at_bottom);
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if *dragging {
                jump_scroll_to(m.row, area, range, scroll_back, at_bottom);
            }
        }
        MouseEventKind::Up(_) => {
            *dragging = false;
        }
        _ => {}
    }
}

/// range 无效(无溢出)时直接忽略(避免 0 行区除零等)
fn scroll_range_is_irrelevant(range: usize, area: Rect) -> bool {
    range == 0 || area.width == 0 || area.height <= 2
}

/// 把滚动条上的行位置换算成 scroll_back(点击/拖动位置成为拇指所在)。
fn jump_scroll_to(
    row: u16,
    area: Rect,
    range: usize,
    scroll_back: &mut usize,
    at_bottom: &mut bool,
) {
    if range == 0 {
        *scroll_back = 0;
        *at_bottom = true;
        return;
    }
    let inner_rows = area.height.saturating_sub(2) as usize;
    if inner_rows == 0 {
        return;
    }
    // 行坐标 → 可视区内偏移(0 = 最顶行)
    let r = (row as i64 - (area.y as i64 + 1)).clamp(0, inner_rows as i64 - 1) as usize;
    // 拇指中心 ≈ 点击行:sb ≈ range × (1 − r/(inner_rows−1))
    let denom = (inner_rows - 1).max(1);
    let from_top = (range * r) / denom;
    let sb = range.saturating_sub(from_top);
    *scroll_back = sb.min(range);
    *at_bottom = *scroll_back == 0;
}

fn draw_permission(f: &mut ratatui::Frame<'_>, area: Rect, pp: &PermissionPrompt) {
    match &pp.warning {
        // 高危/无法判定:红色弹窗,把解析出的目标/原因一并显示,只给单次放行
        Some(w) => {
            let body = format!("{}\n\n{w}", pp.body);
            draw_confirm_popup(
                f,
                area,
                &pp.title,
                &body,
                "y 允许一次 | n 拒绝 | Esc 取消",
                true,
            );
        }
        None => draw_confirm_popup(
            f,
            area,
            &pp.title,
            &pp.body,
            "y 允许一次 | a 本会话都允许 | n 拒绝 | Esc 取消",
            false,
        ),
    }
}

/// 居中确认弹窗(权限确认与 /clear 等破坏性操作确认共用)。danger=true 用红色,
/// 弹窗略高以容纳多出来的说明行。
fn draw_confirm_popup(
    f: &mut ratatui::Frame<'_>,
    area: Rect,
    title: &str,
    body: &str,
    hint: &str,
    danger: bool,
) {
    let popup_w = area.width.min(90);
    let popup_h = if danger { 14u16 } else { 10u16 };
    let color = if danger { Color::Red } else { Color::Yellow };
    let x = area.x + (area.width.saturating_sub(popup_w)) / 2;
    let y = area.y + (area.height.saturating_sub(popup_h)) / 2;
    let popup = Rect {
        x,
        y,
        width: popup_w,
        height: popup_h,
    };
    f.render_widget(Clear, popup);
    let block = Block::default()
        .title(if danger {
            " 高危命令确认 "
        } else {
            " 操作确认 "
        })
        .borders(ratatui::widgets::Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color));
    let inner = block.inner(popup);
    f.render_widget(block, popup);
    let body = truncate(body, 78);
    let lines = vec![
        Line::from(Span::styled(
            format!("  {title}"),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::raw(body)),
        Line::from(""),
        Line::from(Span::styled(
            format!("  {hint}"),
            Style::default().fg(Color::Cyan),
        )),
    ];
    f.render_widget(Paragraph::new(lines).alignment(Alignment::Left), inner);
}

/// 边框颜色 = 权限模式:询问绿 / 编辑放行天蓝 / 全自动紫 / 超级(YOLO)红
fn mode_color(mode: Mode) -> Color {
    match mode {
        Mode::Ask => Color::Green,
        Mode::AcceptEdits => Color::LightBlue,
        Mode::BypassPermissions => Color::Magenta,
        Mode::Yolo => Color::Red,
    }
}

/// Shift+Tab 循环顺序:询问 → 编辑放行 → 全自动 → 超级(YOLO) → 询问
fn next_mode(mode: Mode) -> Mode {
    match mode {
        Mode::Ask => Mode::AcceptEdits,
        Mode::AcceptEdits => Mode::BypassPermissions,
        Mode::BypassPermissions => Mode::Yolo,
        Mode::Yolo => Mode::Ask,
    }
}

/// 模式的人话描述(切换提示里展示)
/// Shift+Tab 切换权限模式:更新状态栏、通知 agent 生效、留一条反馈
fn cycle_permission_mode(
    current_mode: &mut Mode,
    cmd_tx: &mpsc::UnboundedSender<AgentCmd>,
    items: &mut Vec<MsgItem>,
) {
    let next = next_mode(*current_mode);
    *current_mode = next;
    let _ = cmd_tx.send(AgentCmd::SetMode(next));
    items.push(MsgItem::Notice(format!(
        "🔒 权限模式:{} (Shift+Tab 循环切换)",
        next.hint()
    )));
}

fn fmt_time(epoch_secs: f64) -> String {
    // 简化:显示相对时间(分钟前/小时前/日期)
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let diff = now - epoch_secs;
    if diff < 60.0 {
        format!("{:.0}秒前", diff)
    } else if diff < 3600.0 {
        format!("{:.0}分钟前", diff / 60.0)
    } else if diff < 86400.0 {
        format!("{:.1}小时前", diff / 3600.0)
    } else {
        format!("{:.0}天前", diff / 86400.0)
    }
}

fn truncate(s: &str, max: usize) -> String {
    // 实现走 core 共用的一份(按字符截断,不切坏中文/emoji)
    znaide_core::util::truncate_chars(s, max, "…")
}

#[cfg(test)]
mod key_tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn plain_enter_submits() {
        let input = String::from("hello");
        match handle_text_key(key(KeyCode::Enter, KeyModifiers::NONE)) {
            TextAction::Submit => {
                // 宿主读取 input 后自行清空
                assert_eq!(input, "hello");
            }
            _ => panic!("普通 Enter 应发送"),
        }
    }

    #[test]
    fn shift_enter_newline() {
        let input = String::from("ab");
        match handle_text_key(key(KeyCode::Enter, KeyModifiers::SHIFT)) {
            TextAction::Newline => {}
            _ => panic!("Shift+Enter 应换行"),
        }
        assert_eq!(input, "ab");
    }

    #[test]
    fn alt_enter_newline() {
        match handle_text_key(key(KeyCode::Enter, KeyModifiers::ALT)) {
            TextAction::Newline => {}
            _ => panic!("Alt+Enter 应换行"),
        }
    }

    #[test]
    fn ctrl_j_newline() {
        match handle_text_key(key(KeyCode::Char('j'), KeyModifiers::CONTROL)) {
            TextAction::Newline => {}
            _ => panic!("Ctrl+J 应换行"),
        }
    }

    #[test]
    fn plain_char_inserts() {
        let mut input = String::from("a");
        match handle_text_key(key(KeyCode::Char('b'), KeyModifiers::NONE)) {
            TextAction::Insert(c) => {
                assert_eq!(c, 'b');
                input.push(c);
            }
            _ => panic!("普通字符应插入"),
        }
        assert_eq!(input, "ab");
    }

    #[test]
    fn backspace() {
        assert!(matches!(
            handle_text_key(key(KeyCode::Backspace, KeyModifiers::NONE)),
            TextAction::Backspace
        ));
    }

    /// 模拟真实粘贴流程:粘贴文本(经 Paste 分支的 clean)进入 input,
    /// 再连续 Backspace,验证能逐字符/逐行删除。
    #[test]
    fn paste_then_backspace_deletes() {
        let mut input = String::new();
        // 模拟 Event::Paste 分支的规范化
        let pasted = "line one\r\nline two\n"
            .replace("\r\n", "\n")
            .replace('\r', "\n");
        let mut cur = insert_str_at(&mut input, 0, &pasted);
        assert_eq!(input, "line one\nline two\n");
        assert_eq!(cur, input.len());

        // 连按 4 次 Backspace:删 "\n" 与第二行尾 "owt",剩 "line "
        for _ in 0..4 {
            cur = backspace_at(&mut input, cur);
        }
        assert_eq!(input, "line one\nline ");
    }

    #[test]
    fn cursor_edit_center() {
        // "abcdef" 光标移到 'c' 后插入 'X',再退格删 'X'
        let mut input = String::from("abcdef");
        let mut cur = 3; // 在 'c' 后
        cur = insert_at(&mut input, cur, 'X');
        assert_eq!(input, "abcXdef");
        cur = backspace_at(&mut input, cur);
        assert_eq!(input, "abcdef");
        assert_eq!(cur, 3);
    }

    #[test]
    fn delete_at_cursor() {
        let mut input = String::from("abcdef");
        let cur = 1; // 'b' 处
        let cur = delete_at(&mut input, cur);
        assert_eq!(input, "acdef");
        let _ = cur;
    }

    #[test]
    fn cursor_moves() {
        let input = String::from("aé中"); // 含多字节
        assert_eq!(cursor_next(&input, 0), 'a'.len_utf8());
        assert_eq!(
            cursor_prev(&input, input.len()),
            input.len() - '中'.len_utf8()
        );
    }

    #[test]
    fn shift_tab_mode_cycles() {
        // 循环顺序:询问 → 编辑放行 → 全自动 → 超级(YOLO) → 询问
        assert_eq!(next_mode(Mode::Ask), Mode::AcceptEdits);
        assert_eq!(next_mode(Mode::AcceptEdits), Mode::BypassPermissions);
        assert_eq!(next_mode(Mode::BypassPermissions), Mode::Yolo);
        assert_eq!(next_mode(Mode::Yolo), Mode::Ask);
        // 四档都有可读描述(状态栏/提示用)
        for m in [
            Mode::Ask,
            Mode::AcceptEdits,
            Mode::BypassPermissions,
            Mode::Yolo,
        ] {
            assert!(!m.hint().is_empty());
            assert!(!m.label().is_empty());
        }
        // 边框颜色:询问绿 / 编辑放行天蓝 / 全自动紫 / 超级红,两两互不相同
        let ask_c = mode_color(Mode::Ask);
        let acc_c = mode_color(Mode::AcceptEdits);
        let bp_c = mode_color(Mode::BypassPermissions);
        let yolo_c = mode_color(Mode::Yolo);
        assert_eq!(ask_c, Color::Green);
        assert_eq!(acc_c, Color::LightBlue);
        assert_eq!(bp_c, Color::Magenta);
        assert_eq!(yolo_c, Color::Red);
        assert_ne!(ask_c, acc_c);
        assert_ne!(ask_c, bp_c);
        assert_ne!(ask_c, yolo_c);
        assert_ne!(acc_c, bp_c);
        assert_ne!(acc_c, yolo_c);
        assert_ne!(bp_c, yolo_c);
    }
}

#[cfg(test)]
mod token_tests {
    use super::*;

    #[test]
    fn estimate_mixed_text() {
        // 中文每字 1 token:"你好世界" 4 字
        assert_eq!(est_tokens("你好世界"), 4);
        // 英文约 4 字符/token:16 个 ASCII → 4
        assert_eq!(est_tokens("abcdefghijklmnop"), 4);
        // 混合:3 个中文 + 12 个 ASCII → 3 + 3
        assert_eq!(est_tokens("你好吗abcdefghijkl"), 6);
        // 空串
        assert_eq!(est_tokens(""), 0);
    }

    #[test]
    fn token_fmt_abbrev() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(1234), "1.2k");
        assert_eq!(fmt_tokens(12_345), "12.3k");
        assert_eq!(fmt_tokens(2_300_000), "2.3M");
    }

    /// 事件流 → TokenStats 语义:输入/输出按真实 usage 累计;流式增量只进
    /// round_est;真实用量到账后 round_est 清零;回合结束丢弃未结算估算。
    #[test]
    fn session_token_stats_accumulate() {
        let mut items = Vec::new();
        let mut busy = false;
        let mut kind = BusyKind::Work;
        let mut perm = None;
        let mut sid = String::new();
        let mut persona = String::new();
        let mut st = TokenStats::default();

        handle_session_event(
            SessionEvent::TurnStarted,
            &mut items,
            &mut busy,
            &mut kind,
            &mut perm,
            &mut sid,
            &mut persona,
            &mut st,
        );
        assert!(busy);
        assert_eq!(kind, BusyKind::Work);
        handle_session_event(
            SessionEvent::TextDelta("你好".into()),
            &mut items,
            &mut busy,
            &mut kind,
            &mut perm,
            &mut sid,
            &mut persona,
            &mut st,
        ); // 2 字 → 估 2
        assert_eq!(st.round_est, 2);
        // 该轮真实用量到账 → 输入/输出分别累计,估算清零
        handle_session_event(
            SessionEvent::Usage {
                prompt: 100,
                completion: 40,
            },
            &mut items,
            &mut busy,
            &mut kind,
            &mut perm,
            &mut sid,
            &mut persona,
            &mut st,
        );
        assert_eq!(st.input, 100);
        assert_eq!(st.output, 40);
        assert_eq!(st.last_prompt, Some(100)); // 当前上下文占用 = 最近一次 prompt
        assert_eq!(st.round_est, 0);
        handle_session_event(
            SessionEvent::TurnFinished {
                text: String::new(),
                truncated: false,
            },
            &mut items,
            &mut busy,
            &mut kind,
            &mut perm,
            &mut sid,
            &mut persona,
            &mut st,
        );
        assert!(!busy);
        assert_eq!(kind, BusyKind::Work);

        // 跨回合:累计保留;新回合从 0 起估
        handle_session_event(
            SessionEvent::TurnStarted,
            &mut items,
            &mut busy,
            &mut kind,
            &mut perm,
            &mut sid,
            &mut persona,
            &mut st,
        );
        handle_session_event(
            SessionEvent::TextDelta("hello world".into()),
            &mut items,
            &mut busy,
            &mut kind,
            &mut perm,
            &mut sid,
            &mut persona,
            &mut st,
        ); // 11 ASCII → 估 3
        assert_eq!(st.round_est, 3);
        // 端点无 usage:回合结束丢弃未结算估算,累计不变
        handle_session_event(
            SessionEvent::TurnFinished {
                text: String::new(),
                truncated: false,
            },
            &mut items,
            &mut busy,
            &mut kind,
            &mut perm,
            &mut sid,
            &mut persona,
            &mut st,
        );
        assert_eq!(st.round_est, 0);
        assert_eq!(st.input, 100);
        assert_eq!(st.output, 40);
    }

    /// 弱网重试：丢弃本轮半截流式内容，只保留最终成功的一次；估算清零并留一条提示
    #[test]
    fn llm_retrying_discards_partial_stream() {
        let mut items = Vec::new();
        let mut busy = false;
        let mut kind = BusyKind::Work;
        let mut perm = None;
        let mut sid = String::new();
        let mut persona = String::new();
        let mut st = TokenStats::default();
        macro_rules! fire {
            ($e:expr) => {
                handle_session_event(
                    $e, &mut items, &mut busy, &mut kind, &mut perm, &mut sid,
                    &mut persona, &mut st,
                )
            };
        }
        fire!(SessionEvent::TurnStarted);
        fire!(SessionEvent::TextDelta("半截输出".into()));
        fire!(SessionEvent::ReasoningDelta("半截思考".into()));
        assert!(st.round_est > 0);
        fire!(SessionEvent::LlmRetrying {
            attempt: 1,
            max: 3,
            reason: "空回复".into(),
        });
        // 半截内容被清掉，只剩一条重试提示；估算清零
        assert_eq!(st.round_est, 0);
        assert_eq!(items.len(), 1);
        assert!(matches!(&items[0], MsgItem::Notice(t) if t.contains("1/3")));
        // 最终成功的那次增量正常追加
        fire!(SessionEvent::TextDelta("完整成功".into()));
        assert!(matches!(&items[1], MsgItem::AssistantStream(s) if s == "完整成功"));
    }

    /// 静默预算毫秒 → 人读:整分钟缩写,否则秒
    #[test]
    fn ms_budget_fmt() {
        assert_eq!(fmt_ms_budget(90_000), "90s");
        assert_eq!(fmt_ms_budget(240_000), "4min");
        assert_eq!(fmt_ms_budget(150_000), "150s");
        assert_eq!(fmt_ms_budget(600_000), "10min");
    }

    /// 工具卡片:ToolStarted 带预算与启动时刻;静默预警逐档变色;完成回填保留计时
    #[test]
    fn tool_card_tracks_budget_and_silent_alert() {
        let mut items = Vec::new();
        let mut busy = false;
        let mut kind = BusyKind::Work;
        let mut perm = None;
        let mut sid = String::new();
        let mut persona = String::new();
        let mut st = TokenStats::default();
        let fire = |e: SessionEvent,
                    items: &mut Vec<MsgItem>,
                    busy: &mut bool,
                    kind: &mut BusyKind,
                    perm: &mut Option<PermissionPrompt>,
                    sid: &mut String,
                    persona: &mut String,
                    st: &mut TokenStats| {
            handle_session_event(e, items, busy, kind, perm, sid, persona, st);
        };
        let tool = |items: &Vec<MsgItem>| match &items[0] {
            MsgItem::Tool {
                name,
                ok,
                started,
                idle_ms,
                silent,
                ..
            } => (name.clone(), *ok, started.is_some(), *idle_ms, *silent),
            _ => panic!("应是 Tool 卡片"),
        };

        fire(
            SessionEvent::ToolStarted {
                name: "run_shell_command".into(),
                args: String::new(),
                idle_ms: Some(90_000),
            },
            &mut items,
            &mut busy,
            &mut kind,
            &mut perm,
            &mut sid,
            &mut persona,
            &mut st,
        );
        assert_eq!(
            tool(&items),
            ("run_shell_command".into(), None, true, Some(90_000), 0)
        );

        // 静默预警:黄 → 红 → 解除
        fire(
            SessionEvent::ToolSilentAlert {
                name: "run_shell_command".into(),
                level: 1,
            },
            &mut items,
            &mut busy,
            &mut kind,
            &mut perm,
            &mut sid,
            &mut persona,
            &mut st,
        );
        assert_eq!(tool(&items).4, 1);
        fire(
            SessionEvent::ToolSilentAlert {
                name: "run_shell_command".into(),
                level: 2,
            },
            &mut items,
            &mut busy,
            &mut kind,
            &mut perm,
            &mut sid,
            &mut persona,
            &mut st,
        );
        assert_eq!(tool(&items).4, 2);
        fire(
            SessionEvent::ToolSilentAlert {
                name: "run_shell_command".into(),
                level: 0,
            },
            &mut items,
            &mut busy,
            &mut kind,
            &mut perm,
            &mut sid,
            &mut persona,
            &mut st,
        );
        assert_eq!(tool(&items).4, 0);

        // 完成回填 ok/output,启动时刻保留(渲染总用时)
        fire(
            SessionEvent::ToolFinished {
                name: "run_shell_command".into(),
                ok: true,
                output: "done".into(),
            },
            &mut items,
            &mut busy,
            &mut kind,
            &mut perm,
            &mut sid,
            &mut persona,
            &mut st,
        );
        assert_eq!(
            tool(&items),
            (
                "run_shell_command".into(),
                Some(true),
                true,
                Some(90_000),
                0
            )
        );
        // 完成后总用时已定格:不再随帧跳动
        assert!(
            matches!(
                &items[0],
                MsgItem::Tool {
                    ok: Some(true),
                    done_secs: Some(_),
                    ..
                }
            ),
            "完成后应定格总用时"
        );
        // 预警只作用于执行中卡片:完成后收到预警不应改已完成卡片
        fire(
            SessionEvent::ToolSilentAlert {
                name: "run_shell_command".into(),
                level: 1,
            },
            &mut items,
            &mut busy,
            &mut kind,
            &mut perm,
            &mut sid,
            &mut persona,
            &mut st,
        );
        assert_eq!(tool(&items).4, 0);
    }

    /// 调用内容摘要:命令类给 command 原文,其它工具原样展示,空参不显示
    #[test]
    fn tool_args_summary_shapes() {
        assert_eq!(
            tool_args_summary("run_shell_command", r#"{"command":"ls -la"}"#),
            "$ ls -la"
        );
        assert_eq!(
            tool_args_summary("run_shell_command", r#"{"command":"pwd","cwd":"/tmp"}"#),
            "$ pwd  (cwd: /tmp)"
        );
        assert_eq!(
            tool_args_summary("read_file", r#"{"path":"src/main.rs"}"#),
            r#"{"path":"src/main.rs"}"#
        );
        assert_eq!(tool_args_summary("list_directory", "{}"), "");
        assert_eq!(tool_args_summary("list_directory", ""), "");
    }

    /// 历史回放:assistant 声明的工具调用与后续 tool 结果配对成一张卡片
    #[test]
    fn history_tool_call_pairs_with_result() {
        use znaide_core::llm::types::{ChatMessage, FunctionCall, ToolCall};
        let mut items = Vec::new();
        let mut names = std::collections::HashMap::new();
        let mut pending = std::collections::HashMap::new();
        let asst = ChatMessage::assistant_with_tool_calls(
            None,
            vec![ToolCall {
                id: "c1".into(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: "run_shell_command".into(),
                    arguments: r#"{"command":"ls -la"}"#.into(),
                },
            }],
        );
        push_history_item(&mut items, &asst, &mut names, &mut pending);
        // 声明先成"待回填"卡片
        assert!(matches!(
            &items[0],
            MsgItem::Tool { name, args, ok: None, output, .. }
                if name == "run_shell_command" && args == r#"{"command":"ls -la"}"# && output.is_empty()
        ));
        let tool = ChatMessage::tool("c1", "(命令退出码 0)");
        push_history_item(&mut items, &tool, &mut names, &mut pending);
        // 结果回填进同一张卡片,不新增
        if let MsgItem::Tool {
            name,
            args,
            ok: Some(true),
            output,
            ..
        } = &items[0]
        {
            assert_eq!(name, "run_shell_command");
            assert_eq!(args, r#"{"command":"ls -la"}"#);
            assert_eq!(output, "(命令退出码 0)");
        } else {
            panic!("应回填为一张完成卡片");
        }
        assert_eq!(items.len(), 1);
    }

    /// 实时输出:执行中累积到卡片 live,完成后清空回落
    #[test]
    fn live_output_appends_then_clears_on_finish() {
        let mut items = Vec::new();
        let mut busy = false;
        let mut kind = BusyKind::Work;
        let mut perm = None;
        let mut sid = String::new();
        let mut persona = String::new();
        let mut st = TokenStats::default();
        let fire = |e: SessionEvent,
                    items: &mut Vec<MsgItem>,
                    busy: &mut bool,
                    kind: &mut BusyKind,
                    perm: &mut Option<PermissionPrompt>,
                    sid: &mut String,
                    persona: &mut String,
                    st: &mut TokenStats| {
            handle_session_event(e, items, busy, kind, perm, sid, persona, st);
        };
        fire(
            SessionEvent::ToolStarted {
                name: "run_shell_command".into(),
                args: "{}".into(),
                idle_ms: None,
            },
            &mut items,
            &mut busy,
            &mut kind,
            &mut perm,
            &mut sid,
            &mut persona,
            &mut st,
        );
        fire(
            SessionEvent::ToolOutputDelta {
                name: "run_shell_command".into(),
                delta: "line1\n".into(),
            },
            &mut items,
            &mut busy,
            &mut kind,
            &mut perm,
            &mut sid,
            &mut persona,
            &mut st,
        );
        fire(
            SessionEvent::ToolOutputDelta {
                name: "run_shell_command".into(),
                delta: "line2\n".into(),
            },
            &mut items,
            &mut busy,
            &mut kind,
            &mut perm,
            &mut sid,
            &mut persona,
            &mut st,
        );
        if let MsgItem::Tool { ok: None, live, .. } = &items[0] {
            assert_eq!(live, "line1\nline2\n");
        } else {
            panic!("执行中卡片应累积实时输出");
        }
        // 找不到执行中卡片(如历史里的旧命令)的 delta 应被忽略
        fire(
            SessionEvent::ToolFinished {
                name: "run_shell_command".into(),
                ok: true,
                output: "done".into(),
            },
            &mut items,
            &mut busy,
            &mut kind,
            &mut perm,
            &mut sid,
            &mut persona,
            &mut st,
        );
        fire(
            SessionEvent::ToolOutputDelta {
                name: "run_shell_command".into(),
                delta: "late\n".into(),
            },
            &mut items,
            &mut busy,
            &mut kind,
            &mut perm,
            &mut sid,
            &mut persona,
            &mut st,
        );
        if let MsgItem::Tool {
            ok: Some(true),
            live,
            ..
        } = &items[0]
        {
            assert!(live.is_empty(), "完成后实时区应清空回落");
        } else {
            panic!("应已完成");
        }
    }

    /// 实时区纯函数:取尾部 N 行;超限丢头保尾
    #[test]
    fn live_tail_helpers() {
        assert_eq!(last_n_lines("a\nb\nc", 2), "b\nc");
        assert_eq!(last_n_lines("a\nb\nc\n", 2), "b\nc");
        assert_eq!(last_n_lines("abc", 5), "abc");
        assert_eq!(last_n_lines("", 5), "");
        let mut s = "x".repeat(200_000);
        cap_live_tail(&mut s);
        assert!(s.len() <= 128 * 1024, "实时缓冲应封顶: {}", s.len());
        // 封顶后仍以新内容为尾
        let mut s2 = "旧".repeat(100_000);
        s2.push_str("新尾巴");
        cap_live_tail(&mut s2);
        assert!(s2.ends_with("新尾巴"), "应保留最新内容");
    }

    /// 压缩状态:CompactionStarted 置忙且场景为 Compacting;完成事件复位
    #[test]
    fn compaction_sets_busy_kind() {
        let mut items = Vec::new();
        let mut busy = false;
        let mut kind = BusyKind::Work;
        let mut perm = None;
        let mut sid = String::new();
        let mut persona = String::new();
        let mut st = TokenStats::default();
        handle_session_event(
            SessionEvent::CompactionStarted,
            &mut items,
            &mut busy,
            &mut kind,
            &mut perm,
            &mut sid,
            &mut persona,
            &mut st,
        );
        assert!(busy);
        assert_eq!(kind, BusyKind::Compacting);
        // 压缩成功 → 摘要卡展示并复位忙态
        handle_session_event(
            SessionEvent::ContextCompacted {
                removed: 6,
                summary: "摘要".into(),
                kept: vec![],
            },
            &mut items,
            &mut busy,
            &mut kind,
            &mut perm,
            &mut sid,
            &mut persona,
            &mut st,
        );
        assert!(!busy);
        assert_eq!(kind, BusyKind::Work);
        assert!(!items.is_empty()); // 摘要卡已展示
    }

    /// 人格切换事件:更新状态栏显示状态(空 = 关闭人格)
    #[test]
    fn persona_changed_updates_display_state() {
        let mut items = Vec::new();
        let mut busy = false;
        let mut kind = BusyKind::Work;
        let mut perm = None;
        let mut sid = String::new();
        let mut persona = String::new();
        let mut st = TokenStats::default();
        handle_session_event(
            SessionEvent::PersonaChanged {
                name: "毒舌损友".into(),
            },
            &mut items,
            &mut busy,
            &mut kind,
            &mut perm,
            &mut sid,
            &mut persona,
            &mut st,
        );
        assert_eq!(persona, "毒舌损友");
        handle_session_event(
            SessionEvent::PersonaChanged {
                name: String::new(),
            },
            &mut items,
            &mut busy,
            &mut kind,
            &mut perm,
            &mut sid,
            &mut persona,
            &mut st,
        );
        assert!(persona.is_empty(), "关闭人格应清空状态栏显示");
    }
}

#[cfg(test)]
mod slash_tests {
    use super::*;

    /// /clear 是破坏性操作:不应直接清空,而是先弹确认框
    #[test]
    fn slash_clear_asks_confirmation_first() {
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel::<AgentCmd>();
        let (update_tx, _update_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let mut items: Vec<MsgItem> = vec![MsgItem::User("旧消息".into())];
        let mut confirm: Option<ConfirmBox> = None;
        let cwd = std::env::temp_dir();
        let r = handle_command(
            "/clear",
            &cmd_tx,
            &update_tx,
            &mut items,
            &cwd,
            &mut confirm,
        );
        assert!(r.is_none());
        // 展示未被直接清空,而是挂起一个确认
        assert!(!items.is_empty());
        assert!(confirm.is_some());
        match confirm.as_ref().map(|c| &c.action) {
            Some(ConfirmAction::ClearSession) => {}
            other => panic!("确认动作应为 ClearSession,got {other:?}"),
        }
    }
}

#[cfg(test)]
mod completion_tests {
    use super::*;

    fn detect_at(input: &str, cursor: usize) -> (usize, String) {
        match completion_detect(input, cursor) {
            Some((CompletionKind::At, ws, typed)) => (ws, typed),
            other => panic!("detect_at({input:?}, cursor={cursor}) 得到 {other:?}"),
        }
    }

    fn detect_slash(input: &str, cursor: usize) -> (usize, String) {
        match completion_detect(input, cursor) {
            Some((CompletionKind::Slash, ws, typed)) => (ws, typed),
            other => panic!("应检测为 slash 补全,got {other:?}"),
        }
    }

    #[test]
    fn detect_line_head_slash() {
        // "/un" 行首 → slash,typed="un",词起点含 '/'
        let (ws, typed) = detect_slash("/un", 3);
        assert_eq!(&"/un"[ws..3], "/un");
        assert_eq!(typed, "un");
        // 前导空白也算行首
        let (ws, typed) = detect_slash("  /sk", 5);
        assert_eq!(typed, "sk");
        assert_eq!(&"  /sk"[ws..5], "/sk");
    }

    #[test]
    fn slash_not_triggered_mid_sentence() {
        // 非首个词(中间位置)不触发 slash 补全
        assert!(completion_detect("看下 /undo 吧", 8).is_none());
    }

    #[test]
    fn detect_at_anywhere() {
        // 任意位置 @ 触发;词起点含 @。"读 @src/mai" = 3+1+1+7 字节
        let (ws, typed) = detect_at("读 @src/mai", 12);
        assert_eq!(typed, "src/mai");
        assert_eq!(&"读 @src/mai"[ws..12], "@src/mai");
        // 仅 @(候选全量)。"读 @" = 3+1+1
        let (_, typed) = detect_at("读 @", 5);
        assert_eq!(typed, "");
    }

    #[test]
    fn email_not_completed() {
        // a@b.com 不是 @ 引用开头 → 不触发。"联系 a@b.com" = 6+1+6
        assert!(completion_detect("联系 a@b.com", 13).is_none());
    }

    #[test]
    fn detect_quoted_at() {
        // @"my file → typed 不含引号,提示继续。"读 @\"my file" = 3+1+1+1+7
        let (_, typed) = detect_at("读 @\"my file", 13);
        assert_eq!(typed, "my file");
        // 已闭合 @"my file" → 不再提示。光标在引号后:3+1+1+1+7+1=14
        assert!(completion_detect("读 @\"my file\" 好", 14).is_none());
    }

    #[test]
    fn quote_only_when_needed() {
        assert_eq!(quote_at_path("main.rs"), "main.rs");
        assert_eq!(quote_at_path("my file.txt"), "\"my file.txt\"");
        assert_eq!(quote_at_path("中文,标点.txt"), "\"中文,标点.txt\"");
    }

    #[test]
    fn at_candidates_lists_dir_and_files() {
        let dir = std::env::temp_dir().join(format!("znaide_cmp_test_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("doc 目录")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "").unwrap();
        std::fs::write(dir.join("README.md"), "").unwrap();
        std::fs::write(dir.join("my file.txt"), "").unwrap();
        std::fs::write(dir.join(".hidden"), "").unwrap();

        let items = at_candidates(&dir, "");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"src/"));
        assert!(labels.contains(&"README.md"));
        assert!(!labels.contains(&".hidden")); // 隐藏默认不列
                                               // 含空格文件:label 裸名,insert 带引号
        let spaced = items.iter().find(|i| i.label == "my file.txt").unwrap();
        assert_eq!(spaced.insert, "\"my file.txt\"");

        // 目录逐层:@doc → doc 目录(label 裸,insert 带引号,因含空格)
        let items = at_candidates(&dir, "doc");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "doc 目录/");
        assert_eq!(items[0].insert, "\"doc 目录/\"");

        // 深入 @src/m:label/insert 都带目录前缀(src/main.rs)
        let items = at_candidates(&dir, "src/m");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "src/main.rs");
        assert_eq!(items[0].insert, "src/main.rs");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn slash_candidates_filter_and_order() {
        let cwd = std::env::temp_dir();
        let items = slash_candidates(&cwd, "un");
        assert!(items.iter().any(|i| i.label == "/undo"));
        let items = slash_candidates(&cwd, "re");
        assert!(items
            .iter()
            .any(|i| i.label == "/resume" && i.hint.contains("历史会话")));
        // typed="" 列出全部内置
        let all = slash_candidates(&cwd, "");
        assert!(all.len() >= 7);
        assert!(all[0].label == "/help"); // 内置顺序保持
    }

    #[test]
    fn accept_replaces_word_and_moves_cursor() {
        // "读 @src/mai 内容":@ 在字节 4,"src/mai" 结束于 12
        let mut input = String::from("读 @src/mai 内容");
        let mut cursor = 12;
        let cm = Completion {
            kind: CompletionKind::At,
            typed: "src/mai".into(),
            word_start: 4,
            items: vec![CompletionItem {
                label: "src/main.rs".into(),
                insert: "src/main.rs".into(),
                hint: "文件".into(),
            }],
            selected: 0,
        };
        accept_completion(&mut input, &mut cursor, &cm);
        assert_eq!(input, "读 @src/main.rs 内容");
        assert_eq!(cursor, 16); // 4 + len("@src/main.rs")=12
    }

    #[test]
    fn accept_at_adds_at_prefix() {
        // "看 @my f":@ 在字节 4,typed "my f" 结束于 9(行尾)→ 采纳自动补空格
        let mut input = String::from("看 @my f");
        let mut cursor = 9;
        let cm = Completion {
            kind: CompletionKind::At,
            typed: "my f".into(),
            word_start: 4,
            items: vec![CompletionItem {
                label: "my file.txt".into(),
                insert: "\"my file.txt\"".into(),
                hint: "文件".into(),
            }],
            selected: 0,
        };
        accept_completion(&mut input, &mut cursor, &cm);
        assert_eq!(input, "看 @\"my file.txt\" ");
        assert_eq!(cursor, 19); // 4 + len("@\"my file.txt\" ")=15
    }

    #[test]
    fn accept_slash_appends_space_for_args() {
        // slash 行尾采纳 → 补空格便于继续输参数;光标位于空格后
        let mut input = String::from("/un");
        let mut cursor = 3;
        let cm = Completion {
            kind: CompletionKind::Slash,
            typed: "un".into(),
            word_start: 0,
            items: vec![CompletionItem {
                label: "/undo".into(),
                insert: "/undo".into(),
                hint: "undo 快照/回滚".into(),
            }],
            selected: 0,
        };
        accept_completion(&mut input, &mut cursor, &cm);
        assert_eq!(input, "/undo ");
        assert_eq!(cursor, 6);
    }

    #[test]
    fn accept_dir_keeps_traversing_no_space() {
        // 目录采纳(@doc → doc 目录/):不补空格,可继续深入子层
        // "读 @doc":@ 在字节 4,doc 结束于 8
        let mut input = String::from("读 @doc");
        let mut cursor = 8;
        let cm = Completion {
            kind: CompletionKind::At,
            typed: "doc".into(),
            word_start: 4,
            items: vec![CompletionItem {
                label: "doc 目录/".into(),
                insert: "\"doc 目录/\"".into(),
                hint: "目录".into(),
            }],
            selected: 0,
        };
        accept_completion(&mut input, &mut cursor, &cm);
        assert_eq!(input, "读 @\"doc 目录/\"");
        assert_eq!(cursor, 18); // 4 + len("@\"doc 目录/\"")=14
    }

    #[test]
    fn accept_no_double_space_when_followed_by_space() {
        // 词后原本就有空白 → 不再补,避免双空格
        let mut input = String::from("读 @src/mai 内容");
        let mut cursor = 12;
        let cm = Completion {
            kind: CompletionKind::At,
            typed: "src/mai".into(),
            word_start: 4,
            items: vec![CompletionItem {
                label: "src/main.rs".into(),
                insert: "src/main.rs".into(),
                hint: "文件".into(),
            }],
            selected: 0,
        };
        accept_completion(&mut input, &mut cursor, &cm);
        assert_eq!(input, "读 @src/main.rs 内容");
        assert_eq!(cursor, 16); // 4 + len("@src/main.rs")=12
    }

    #[test]
    fn ghost_tail_only_suffix() {
        let mk = |kind: CompletionKind, typed: &str, label: &str| Completion {
            kind,
            typed: typed.into(),
            word_start: 0,
            items: vec![CompletionItem {
                label: label.into(),
                insert: label.into(),
                hint: String::new(),
            }],
            selected: 0,
        };
        // slash:typed 不含 '/',ghost 补 label 剩余
        assert_eq!(
            ghost_tail(&mk(CompletionKind::Slash, "he", "/help")).as_deref(),
            Some("lp")
        );
        // 已完全输入 → 无 ghost
        assert_eq!(
            ghost_tail(&mk(CompletionKind::Slash, "help", "/help")),
            None
        );
        // @ 补全目录尾巴(label 带目录前缀)
        assert_eq!(
            ghost_tail(&mk(CompletionKind::At, "src/ma", "src/main.rs")).as_deref(),
            Some("in.rs")
        );
    }
}

#[cfg(test)]
mod input_metrics_tests {
    use super::*;

    /// 输入区动态高度的物理行计算(与渲染共用 wrap_plain,列宽口径)
    #[test]
    fn phys_lines_basic() {
        assert_eq!(input_phys_lines("", 30), 1); // 空输入保底 1 行
        assert_eq!(input_phys_lines("abc", 30), 1);
        assert_eq!(input_phys_lines("a\nb", 30), 2);
        assert_eq!(input_phys_lines("a\n\nb", 30), 3);
        assert_eq!(input_phys_lines("abc\n", 30), 2); // 结尾换行 → 末尾空行也算
    }

    #[test]
    fn phys_lines_wrap_by_display_width() {
        // ASCII 每列 1;16 列文本在 8 列宽下折成 2 行
        assert_eq!(input_phys_lines("abcdefghijklmnop", 8), 2);
        // 中文双宽:4 字 = 8 列;body_w=4 → 每行 2 字 → 2 行
        assert_eq!(input_phys_lines("你好世界", 4), 2);
        assert_eq!(input_phys_lines("你好世界", 8), 1);
        // 多逻辑行各自折行后相加
        assert_eq!(input_phys_lines("abcdefghijklmnop\nabcdefghijklmnop", 8), 4);
    }

    #[test]
    fn input_prefix_alignment() {
        let (f, c) = input_prefix("");
        assert_eq!(f, "❯ ");
        assert_eq!(c, "  ");
        let (f2, c2) = input_prefix(" [等待确认]");
        assert_eq!(f2.chars().count(), c2.chars().count()); // 续行缩进与首行前缀等宽
    }
}

#[cfg(test)]
mod sanitize_tests {
    use super::*;

    #[test]
    fn strips_full_csi_mouse_sequence() {
        // 完整 SGR 鼠标序列(拖选/拖滚动条时的字节流形态)
        let s = "\x1b[<35;110;1M 你好";
        assert_eq!(sanitize_typed_text(s), " 你好");
    }

    #[test]
    fn strips_bare_escape_and_control() {
        // 孤立 ESC + 控制字符过滤;换行/制表保留
        let s = "a\x1b\x07b\x01c\n\td";
        assert_eq!(sanitize_typed_text(s), "abc\n\td");
    }

    #[test]
    fn keeps_normal_text_and_multiline() {
        let s = "读 @README.md\n粘贴正文";
        assert_eq!(sanitize_typed_text(s), s);
    }
}

#[cfg(test)]
mod anim_tests {
    use super::*;

    /// 流动光条:段数正确、相位推进时内容变化(有"流动"感)
    #[test]
    fn wave_row_length_and_motion() {
        let w0 = wave_row(0, 8);
        let w1 = wave_row(1, 8);
        assert_eq!(w0.chars().count(), 8);
        assert_eq!(w1.chars().count(), 8);
        assert_ne!(w0, w1, "相位推进应有可见变化");
        // 全由光条字符构成
        assert!(w0.chars().all(|c| BAR_LEVELS.contains(&c)));
        // 小段数(消息区底部提示)也正常
        assert_eq!(wave_row(0, 4).chars().count(), 4);
    }

    /// 脉动光标:周期内循环、两帧间可能同字符(慢半拍)但整体覆盖多档
    #[test]
    fn pulse_char_cycles() {
        let seen: std::collections::HashSet<char> = (0..16).map(|f| pulse_char(f)).collect();
        assert!(seen.len() >= 4, "8 帧周期内应出现多个档位,实际 {seen:?}");
        assert_eq!(pulse_char(8), pulse_char(0)); // 周期 8
    }

    /// Logo 字形回归:拼装结果必须是 ZNAIDE 风格(7 行、首字母斜线从上右向左下)
    #[test]
    fn logo_spells_znaide() {
        let lines = logo_lines();
        assert_eq!(lines.len(), 7);
        // 首字母 Z:顶行满横、第二行从右(带缩进)起;首/末行右缘都是字母 E 的右臂
        assert!(lines[0].starts_with("░█████████"));
        assert!(lines[1].trim_start().starts_with("░██"));
        assert!(lines[0].ends_with("░██████████"));
        assert!(lines[6].ends_with("░██████████"));
    }
}

#[cfg(test)]
mod ctx_usage_tests {
    use super::*;

    /// 上下文占用徽标:只要窗口已知就常驻显示;阈值变色;没测到的占用画 `?` 不画 0%
    #[test]
    fn badge_usage_and_colors() {
        // 窗口为 0(配置里写了无效值)才不显示
        assert!(ctx_usage_badge(Some(100), Some(0)).is_none());
        // /clear 之后是真的空 → 老实画 0%
        let (t, c) = ctx_usage_badge(Some(0), Some(40_960)).unwrap();
        assert!(t.contains("0%"));
        assert!(t.matches('░').count() == 10);
        assert_eq!(c, Color::DarkGray);
        // 约 25% → 5 格窗口 10 格内应填 2-3 格(round:25% → 2.5 → 3)
        let (t, c) = ctx_usage_badge(Some(10_240), Some(40_960)).unwrap();
        assert!(t.starts_with("ctx "));
        assert!(t.contains("25%"));
        assert!(t.matches('█').count() >= 2 && t.matches('█').count() <= 3);
        assert_eq!(c, Color::DarkGray);
        // 70%+ → 黄
        let (_, c) = ctx_usage_badge(Some(28_672), Some(40_960)).unwrap(); // 70%
        assert_eq!(c, Color::Yellow);
        // 90%+ → 红
        let (t, c) = ctx_usage_badge(Some(37_000), Some(40_960)).unwrap();
        assert_eq!(c, Color::Red);
        assert!(t.contains('█'));
        // 超窗保护:百分比封顶显示但不越界 panic
        let (t, _) = ctx_usage_badge(Some(1_000_000), Some(40_960)).unwrap();
        assert!(t.ends_with("100%"));
    }

    /// B:占用还没测到(刚 /resume 或 /compact 完)时画 `ctx ?`,不画 0%——
    /// 恢复出来的历史明明占着上下文,空条会让人以为还能再塞很多
    #[test]
    fn badge_unknown_usage_is_not_zero_percent() {
        let (t, c) = ctx_usage_badge(None, Some(40_960)).unwrap();
        assert_eq!(t, "ctx ?", "窗口已知但占用未知:应如实说没测到,而不是 0%");
        assert!(!t.contains('%'));
        assert!(!t.contains('░'));
        assert_eq!(c, Color::DarkGray, "未知占用不该被当成红/黄");
        // 窗口也未知时同样只是"没数",不编绝对量
        let (t2, _) = ctx_usage_badge(None, None).unwrap();
        assert!(t2.contains('?'));
        assert!(!t2.contains('~'));
    }

    /// B:新会话 / `/clear` 之后画真实的 0%;刚 `/resume` 完则是"未知"(见上一个用例)
    #[test]
    fn new_session_starts_at_zero_but_resume_is_unknown() {
        let mut st = TokenStats::default();
        assert_eq!(st.last_prompt, Some(0), "新会话里没有对话,就是 0");
        let mut items = Vec::new();
        let mut busy = false;
        let mut kind = BusyKind::Work;
        let mut perm = None;
        let mut sid = String::new();
        let mut persona = String::new();
        handle_session_event(
            SessionEvent::HistoryLoaded {
                count: 1,
                messages: vec![znaide_core::llm::types::ChatMessage::user("hi")],
            },
            &mut items,
            &mut busy,
            &mut kind,
            &mut perm,
            &mut sid,
            &mut persona,
            &mut st,
        );
        assert_eq!(
            st.last_prompt, None,
            "resume 出来的历史占着上下文,占多少要等下一轮 usage 才算得出"
        );
    }

    /// B:窗口未知时只报真实绝对量,不编百分比、不变红(猜错的窗口比没窗口更误事)
    #[test]
    fn badge_unknown_window_reports_absolute_only() {
        let (t, c) = ctx_usage_badge(Some(21_400), None).unwrap();
        assert!(t.contains("21.4k"), "未知窗口也要给出真实绝对量: {t}");
        assert!(!t.contains('%'), "不该编百分比: {t}");
        assert_eq!(c, Color::DarkGray, "未知窗口不参与红/黄升级");
        // 还没有真实 usage 时如实说"窗口未知"
        let (t0, _) = ctx_usage_badge(Some(0), None).unwrap();
        assert!(t0.contains('?'));
    }

    /// 工具输出入库封顶:短的原样留,长的保头并说明(卡片只渲染前 500 字符)
    #[test]
    fn tool_output_capped_at_ingest() {
        let short = "ok".to_string();
        assert_eq!(cap_tool_output(short.clone()), short);
        // 正好等于上限:不截断
        let edge = "a".repeat(TOOL_OUT_CAP);
        assert_eq!(cap_tool_output(edge.clone()), edge);
        // 超限:保头 + 标记;多字节字符不能被切坏
        let long = "中".repeat(TOOL_OUT_CAP / 3 + 100);
        let capped = cap_tool_output(long.clone());
        assert!(capped.starts_with('中'), "要保头");
        assert!(capped.contains("未留存"), "要有截断说明");
        assert!(capped.chars().count() < long.chars().count());
    }

    /// 轮数上限优先级:命令行 --max-turns 覆盖 config(0 = 不限,不能被换成默认)
    #[test]
    fn cli_max_turns_overrides_config() {
        assert_eq!(effective_max_turns(Some(5)), 5);
        assert_eq!(effective_max_turns(Some(0)), 0);
    }
}

#[cfg(test)]
mod input_queue_tests {
    use super::*;

    /// 排队消息按 FIFO 逐条执行,每条配额一个完整回合(不合并)
    #[test]
    fn queued_input_runs_in_fifo_order() {
        let mut q = InputQueue::default();
        assert!(q.is_empty());
        assert!(q.pop_ready(true, false).is_none(), "空队列取出应为 None");
        let _ = q.push("第一条");
        let _ = q.push("第二条");
        assert_eq!(q.len(), 2);
        assert_eq!(q.pop_ready(true, false).as_deref(), Some("第一条"));
        assert_eq!(q.pop_ready(true, false).as_deref(), Some("第二条"));
        assert!(q.is_empty());
    }

    /// 忙 / 弹窗期间一律不放行:排队消息不打断正在跑的回合;
    /// 回合结束(含被 Esc 中断、出错中止,都落到 busy=false)且无弹窗才接手
    #[test]
    fn queued_input_waits_while_busy_or_modal() {
        let mut q = InputQueue::default();
        let _ = q.push("等这轮跑完");
        assert!(q.pop_ready(false, false).is_none(), "回合还在跑,不能放行");
        assert!(q.pop_ready(true, true).is_none(), "弹窗挂着,不能放行");
        assert_eq!(q.len(), 1, "放行不了时队列必须原样留着");
        assert_eq!(q.pop_ready(true, false).as_deref(), Some("等这轮跑完"));
    }

    /// 入队提示:多行压成一行、超长按字符截断(不切坏中文),并说清什么时候发
    #[test]
    fn queued_notice_previews_on_one_line() {
        let q = InputQueue::default();
        let notice = q.push("第一行\n第二行\n第三行");
        assert!(
            notice.contains("第一行 第二行"),
            "多行预览要压成一行: {notice}"
        );
        assert!(
            !notice.contains('\n'),
            "提示本身不能是消息区里的多行: {notice}"
        );
        assert!(
            notice.contains("轮边界"),
            "要说清发出时机是轮边界: {notice}"
        );
        let long = q.push(&"很长的一段话".repeat(20));
        assert!(long.ends_with('…'), "超长要带省略号: {long}");
        assert!(long.chars().count() < 80, "预览不该把整段贴回来: {long}");
        assert_eq!(q.len(), 2);
    }

    /// 队列只有一份存储:引擎(会话侧)在轮边界取走一条后,界面这边的"队列 N"自动减少
    #[test]
    fn queue_count_shares_one_source_with_engine() {
        let shared: SharedInbox = std::sync::Arc::default();
        let q = InputQueue::new(shared.clone());
        let _ = q.push("第一条");
        let _ = q.push("第二条");
        assert_eq!(q.len(), 2);
        // 模拟 core 的轮边界取走一条(它拿的是同一个投递箱)
        assert_eq!(
            shared.lock().unwrap().pop_front().as_deref(),
            Some("第一条")
        );
        assert_eq!(q.len(), 1, "界面计数必须跟着引擎走,不能各记一份");
        assert!(!q.is_empty());
        assert_eq!(
            shared.lock().unwrap().pop_front().as_deref(),
            Some("第二条")
        );
        assert!(q.is_empty(), "最后一条也被插走后,界面不该再显示队列");
    }
}
