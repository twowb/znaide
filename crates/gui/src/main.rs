#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use clap::Parser;
use tokio::sync::{broadcast, mpsc};
use znaide_core::config::{Config, ProxyConfig, ProxyMode};
use znaide_core::permissions::Mode;
use znaide_core::session::SessionEvent;
use znaide_host::agent::{AgentCmd, AgentConfig, AgentHandle, HostEvent};
use znaide_host::policy::RetryOverride;
use znaide_host::{NoteLevel, StartupArgs};

use znaide_gui::bridge::{self, Hello, LiveState};
use znaide_gui::server::{self, AppState, UiInput};
use znaide_gui::settings;

mod diag;
#[cfg(feature = "window")]
mod window;

const BUILD_STAMP: &str = "xiufan.0.0.1";
pub const PRODUCT: &str = "修凡AI全能助手";
pub const GUI_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const ENGINE_VERSION: &str = znaide_core::VERSION;

fn full_title() -> String {
    format!("{PRODUCT} V{GUI_VERSION}（基于 znaide 引擎 V{ENGINE_VERSION}）")
}

#[derive(Parser, Debug, Clone)]
#[command(
    name = "xiufan",
    version = env!("CARGO_PKG_VERSION"),
    about = "修凡AI全能助手（基于 znaide 引擎）"
)]
struct Cli {
    /// 本地服务端口（0 = 由系统分配）
    #[arg(long, value_name = "PORT", default_value_t = 7645)]
    port: u16,

    /// 强制浏览器模式：只起服务并打印链接（不打开原生窗口）
    #[arg(long)]
    no_window: bool,

    /// 工作目录（缺省为当前目录）
    #[arg(long, value_name = "DIR")]
    cwd: Option<PathBuf>,

    /// 权限模式:ask / acceptEdits / bypassPermissions / yolo(默认 ask)
    #[arg(long, value_name = "MODE", default_value = "ask")]
    permission: String,

    /// 覆盖模型名
    #[arg(long, value_name = "MODEL")]
    model: Option<String>,

    /// 覆盖 OpenAI 兼容端点
    #[arg(long, value_name = "URL")]
    base_url: Option<String>,

    /// 覆盖 API key
    #[arg(long, value_name = "KEY")]
    api_key: Option<String>,

    /// 选择 provider 预设
    #[arg(long, value_name = "NAME")]
    provider: Option<String>,

    /// 打开时恢复指定历史会话（会话 ID 或文件名片段）
    #[arg(long, value_name = "TARGET")]
    resume: Option<String>,

    /// 单条消息最多允许的模型往返轮数（0 = 不限）
    #[arg(long, value_name = "N")]
    max_turns: Option<usize>,

    /// 网络代理（模型请求、网页抓取、更新下载统一走它）。当次生效、不写盘
    #[arg(long, value_name = "URL")]
    proxy: Option<String>,

    /// 强制直连：忽略配置文件与环境变量里的代理
    #[arg(long)]
    no_proxy: bool,

    /// 弱网重试：一次模型调用失败后最多再试几次（0..=8）
    #[arg(long, value_name = "N")]
    retry: Option<usize>,

    /// 关闭弱网重试
    #[arg(long)]
    no_retry: bool,

    /// 本次运行追加一个预设请求头（`名字=值`，可重复）
    #[arg(long = "extra-header", value_name = "K=V")]
    extra_header: Vec<String>,

    /// 关闭全部预设请求头（含配置文件里的）
    #[arg(long)]
    no_extra_headers: bool,
}

fn main() {
    // 先接控制台/装 panic 钩子:后面任何失败都要留下痕迹,不能静默闪退
    diag::init();
    if let Err(e) = run() {
        diag::fatal(&format!("{e:#}"));
    }
}

fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    diag::log(format!(
        "参数:port={} no_window={} cwd={:?} resume={:?}",
        cli.port, cli.no_window, cli.cwd, cli.resume
    ));
    if let Some(p) = &cli.cwd {
        if !p.is_dir() {
            diag::warn(format!("⚠ --cwd 不存在,改用默认目录:{}", p.display()));
        }
    }
    // 单实例:同一数据目录只允许一个 GUI(文件锁由内核持有,进程退出自动释放)
    let _instance = acquire_instance_lock()?;
    diag::log("单实例锁已获取");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let Some(running) = rt.block_on(start(cli.clone()))? else {
        diag::log("start() 返回无窗口模式收尾");
        return Ok(());
    };

    #[cfg(feature = "window")]
    if !cli.no_window {
        // 页面 /quit 或 Ctrl+C → 关闭窗口
        let (tx, rx) = std::sync::mpsc::channel();
        {
            let sd = running.shutdown.clone();
            rt.spawn(async move {
                sd.notified().await;
                let _ = tx.send(());
            });
        }
        {
            let sd = running.shutdown.clone();
            rt.spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    sd.notify_one();
                }
            });
        }
        diag::log("打开原生窗口");
        match window::run(&running.url, rx) {
            Ok(()) => diag::log("窗口已关闭"),
            Err(e) => {
                diag::warn(format!("⚠ 原生窗口打开失败({e});改用系统浏览器"));
                diag::log(format!("原生窗口失败,退回浏览器:{e}"));
                open_browser(&running.url);
                rt.block_on(wait_for_exit(&running.shutdown))?;
            }
        }
        if let Some(agent) = current(&running.agent) {
            agent.shutdown();
        }
        return Ok(());
    }

    open_browser(&running.url);
    rt.block_on(wait_for_exit(&running.shutdown))?;
    if let Some(agent) = current(&running.agent) {
        agent.shutdown();
    }
    Ok(())
}

/// `--no-window` 或原生窗口起不来时的浏览器模式:等 Ctrl+C / 页面 `/quit`。
async fn wait_for_exit(shutdown: &Arc<tokio::sync::Notify>) -> anyhow::Result<()> {
    tokio::select! {
        r = tokio::signal::ctrl_c() => { r?; }
        _ = shutdown.notified() => {}
    }
    Ok(())
}

async fn start(cli: Cli) -> anyhow::Result<Option<Running>> {
    let cli = &cli;

    let proxy_ov = znaide_core::config::ProxyOverride {
        url: cli.proxy.clone(),
        no_proxy: cli.no_proxy,
    };
    let mut header_ov = znaide_core::config::HeaderOverride {
        add: Vec::new(),
        no_headers: cli.no_extra_headers,
    };
    for raw in &cli.extra_header {
        match raw.split_once('=') {
            Some((k, v)) if !k.trim().is_empty() => {
                header_ov.add.push((k.trim().to_string(), v.to_string()));
            }
            _ => diag::warn(format!("⚠ --extra-header 要写成 名字=值,已忽略:{raw}")),
        }
    }

    let mode = Mode::parse(&cli.permission)?;

    // 与 TUI 同一条启动准备序列:数据目录/内置资源/迁移/配置/代理与头/首启/恢复/重试
    let prepared = znaide_host::prepare(StartupArgs {
        cwd: settings::resolve_start_cwd(cli.cwd.clone()),
        model: cli.model.clone(),
        base_url: cli.base_url.clone(),
        api_key: cli.api_key.clone(),
        provider: cli.provider.clone(),
        proxy: proxy_ov,
        headers: header_ov,
        retry: RetryOverride {
            cli: cli.retry,
            no_retry: cli.no_retry,
        },
        max_turns: cli.max_turns,
        resume_target: cli.resume.clone(),
        config_provided: config_provided(cli),
    })?;

    settings::save_last_cwd(&prepared.cwd);
    for n in &prepared.notes {
        match n.level {
            NoteLevel::Warn => diag::warn(format!("⚠ {}", n.text)),
            NoteLevel::Info => diag::warn(&n.text),
        }
    }
    // --resume 找不到:打印候选清单并正常退出(与 CLI 一致)
    if let Some(e) = &prepared.resume_error {
        diag::warn(e);
        diag::log(format!("--resume 未命中:{e}"));
        return Ok(None);
    }

    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<HostEvent>();

    // 引擎槽:首启时没有,配置向导保存成功后再补建(事件通道从启动起就是同一条)
    let agent: Arc<Mutex<Option<Arc<AgentHandle>>>> = Arc::default();
    if let Some(resolved) = &prepared.resolved {
        let resume_session_id = prepared
            .resume
            .as_ref()
            .and_then(|p| p.file_stem())
            .map(|s| s.to_string_lossy().to_string());
        let handle = znaide_host::agent::spawn(
            AgentConfig {
                resolved: resolved.clone(),
                cwd: prepared.cwd.clone(),
                mode,
                persona: prepared.persona.clone(),
                max_turns: prepared.max_turns,
                retry: prepared.retry_override,
                resume_session_id,
            },
            ev_tx.clone(),
        )?;
        // 启动即恢复目标历史会话(HistoryLoaded 事件会把界面整块替换为恢复内容)
        if let Some(rp) = &prepared.resume {
            let _ = handle.send(AgentCmd::LoadHistory(rp.clone()));
        }
        *agent.lock().unwrap() = Some(Arc::new(handle));
    }

    // 下行总线:宿主事件 → 所有已连接页面
    let (bus, _) = broadcast::channel::<String>(2048);
    let live: Arc<Mutex<LiveState>> = Arc::default();
    // 顶栏状态:权限档/模型/服务商/人格,改动后同步给页面并记住(重连补发)
    let status = Arc::new(Mutex::new(bridge::Status {
        model: prepared
            .resolved
            .as_ref()
            .map(|r| r.model.clone())
            .unwrap_or_default(),
        provider: prepared
            .resolved
            .as_ref()
            .map(|r| r.provider_name.clone())
            .unwrap_or_default(),
        base_url: prepared
            .resolved
            .as_ref()
            .map(|r| r.base_url.clone())
            .unwrap_or_default(),
        api_key_set: prepared
            .resolved
            .as_ref()
            .and_then(|r| r.api_key.as_ref())
            .is_some(),
        configured: !prepared.first_run,
        mode: mode_key(mode).to_string(),
        mode_label: mode.label().to_string(),
        persona: prepared.persona.clone(),
        ctx_window: prepared
            .resolved
            .as_ref()
            .and_then(|r| r.effective_context_window()),
    }));
    live.lock().unwrap().status = Some(status.lock().unwrap().clone());
    live.lock().unwrap().sessions = bridge::session_frames();
    live.lock().unwrap().cwd = Some(prepared.cwd.display().to_string());

    // 启动时静默探测新版本:有则顶栏出提示点(页面没连上时先记在 live,hello 会补发)
    tokio::spawn({
        let bus = bus.clone();
        let live = live.clone();
        let proxy = prepared.proxy.clone();
        async move {
            if let Some(msg) =
                znaide_host::update::check_notice_for(&proxy, GUI_VERSION, "gui-v").await
            {
                live.lock().unwrap().update_notice = Some(msg.clone());
                let _ = bus.send(bridge::update_available_frame(&msg));
            }
        }
    });

    tokio::spawn({
        let bus = bus.clone();
        let live = live.clone();
        async move {
            while let Some(ev) = ev_rx.recv().await {
                if let Some(frame) = bridge::frame(&ev) {
                    if let HostEvent::Ask(_) = &ev {
                        live.lock().unwrap().pending_ask = Some(frame.clone());
                    }
                    if let HostEvent::Session(SessionEvent::SessionInfo { id, .. }) = &ev {
                        live.lock().unwrap().session_id = Some(id.clone());
                    }
                    let _ = bus.send(frame);
                }
                // 回合结束:侧边栏的标题/时间/消息数会变(写盘后再扫)
                if matches!(ev, HostEvent::Session(SessionEvent::TurnFinished { .. })) {
                    let bus = bus.clone();
                    let live = live.clone();
                    tokio::task::spawn_blocking(move || refresh_sessions(&bus, &live));
                }
            }
        }
    });

    // 页面动作 → 宿主动作(权限回答仍由 host 二次校验)
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiInput>();
    // `/quit` 的退出信号(与 Ctrl+C 走同一条收尾)
    let shutdown = Arc::new(tokio::sync::Notify::new());
    // 命令面需要发送端;引擎还没起来时用一个没人接收的通道,`/help` `/skills` 仍可用
    let (dead_tx, _dead_rx) = mpsc::unbounded_channel::<AgentCmd>();
    tokio::spawn({
        let bus = bus.clone();
        let agent = agent.clone();
        let dead_tx = dead_tx;
        let ev_tx = ev_tx.clone();
        let mut cwd = prepared.cwd.clone();
        let proxy = prepared.proxy.clone();
        let cfg = prepared.cfg.clone();
        let status = status.clone();
        let live = live.clone();
        let shutdown = shutdown.clone();
        let max_turns = prepared.max_turns;
        let retry = prepared.retry_override;
        async move {
            let mut cfg = cfg;
            fn fail(bus: &broadcast::Sender<String>, msg: &str) {
                let _ = bus.send(bridge::error_frame(msg));
            }
            const NO_ENGINE: &str = "还没有可用的模型配置。先运行一次 `znaide` 完成配置向导。";
            while let Some(input) = ui_rx.recv().await {
                match input {
                    UiInput::Prompt(text) => match current(&agent).as_ref() {
                        Some(a) => {
                            if !a.send(AgentCmd::Prompt(text)) {
                                fail(&bus, "引擎已退出,消息没有送达。");
                            }
                        }
                        None => fail(&bus, NO_ENGINE),
                    },
                    UiInput::Queue(text) => match current(&agent).as_ref() {
                        Some(a) => match a.inbox().lock() {
                            Ok(mut q) => q.push_back(text),
                            Err(_) => fail(&bus, "排队失败(内部锁异常),请重发。"),
                        },
                        None => fail(&bus, NO_ENGINE),
                    },
                    UiInput::Unqueue(text) => match current(&agent).as_ref() {
                        Some(a) => {
                            let removed = match a.inbox().lock() {
                                Ok(mut q) => match q.iter().position(|t| *t == text) {
                                    Some(i) => {
                                        q.remove(i);
                                        true
                                    }
                                    None => false,
                                },
                                Err(_) => false,
                            };
                            if !removed {
                                fail(&bus, "这条排队消息已被引擎取走或已不存在。");
                            }
                        }
                        None => fail(&bus, NO_ENGINE),
                    },
                    UiInput::Cancel => {
                        if let Some(a) = current(&agent).as_ref() {
                            a.cancel_round();
                        }
                    }
                    UiInput::Drain => {
                        let Some(a) = current(&agent) else { continue };
                        // 回合结束后仍有排队:取一条当作新回合(TUI 空闲时同样从 inbox 取)
                        let next = a.inbox().lock().ok().and_then(|mut q| q.pop_front());
                        if let Some(text) = next {
                            if !a.send(AgentCmd::Prompt(text)) {
                                fail(&bus, "引擎已退出,排队消息没有送达。");
                            }
                        }
                    }
                    UiInput::Command(raw) => {
                        let cmd_tx = current(&agent)
                            .map(|a| a.cmd_tx())
                            .unwrap_or_else(|| dead_tx.clone());
                        let res = znaide_host::command::run(&raw, &cmd_tx, &cwd);
                        for (line, text) in res.lines {
                            let kind = match line {
                                znaide_host::command::Line::Notice => "notice",
                                znaide_host::command::Line::Output => "output",
                            };
                            let _ = bus.send(bridge::command_frame(kind, &text));
                        }
                        match res.outcome {
                            znaide_host::command::Outcome::Handled => {}
                            znaide_host::command::Outcome::NeedConfirmClear { title, body } => {
                                let _ = bus.send(bridge::confirm_frame("clear", &title, &body));
                            }
                            znaide_host::command::Outcome::OpenSessions => {
                                // 无参 /resume:展开左栏(第 4 步的可视化落点)
                                let _ = bus.send(bridge::open_sessions_frame());
                            }
                            znaide_host::command::Outcome::Quit => {
                                let _ = bus.send(bridge::shutdown_frame());
                                shutdown.notify_one();
                                break;
                            }
                            znaide_host::command::Outcome::Compact => {
                                match current(&agent).as_ref() {
                                    Some(a) => {
                                        a.send(AgentCmd::Compact);
                                    }
                                    None => fail(&bus, NO_ENGINE),
                                }
                            }
                            znaide_host::command::Outcome::Task(text) => {
                                match current(&agent).as_ref() {
                                    Some(a) => {
                                        a.send(AgentCmd::Prompt(text));
                                    }
                                    None => fail(&bus, NO_ENGINE),
                                }
                            }
                            znaide_host::command::Outcome::RunSkill { name, args } => {
                                match current(&agent).as_ref() {
                                    Some(a) => {
                                        a.send(AgentCmd::RunSkill { name, args });
                                    }
                                    None => fail(&bus, NO_ENGINE),
                                }
                            }
                            znaide_host::command::Outcome::RunUpdate => {
                                // 下载可能很久:后台任务,结果回到消息流
                                let bus = bus.clone();
                                let proxy = proxy.clone();
                                tokio::spawn(async move {
                                    let msg = znaide_host::update::install(&proxy).await;
                                    let _ = bus.send(bridge::command_frame("notice", &msg));
                                });
                            }
                        }
                    }
                    UiInput::SetCwd(path) => {
                        let p = std::path::PathBuf::from(&path);
                        let p = std::fs::canonicalize(&p).unwrap_or(p);
                        if !p.is_dir() {
                            fail(&bus, &format!("目录不可用:{}", p.display()));
                            continue;
                        }
                        if let Some(a) = current(&agent).as_ref() {
                            a.send(AgentCmd::SetCwd(p.clone()));
                        }
                        cwd = p.clone();
                        live.lock().unwrap().cwd = Some(cwd.display().to_string());
                        settings::save_last_cwd(&cwd);
                        let _ = bus.send(bridge::cwd_frame(&cwd.display().to_string()));
                        send_settings(&bus, &cwd, &cfg, &status, &live);
                        let _ = bus.send(bridge::command_frame(
                            "notice",
                            &format!("工作目录已切换到 {}", cwd.display()),
                        ));
                    }
                    UiInput::ListPaths(prefix) => {
                        let bus = bus.clone();
                        let cwd = cwd.clone();
                        tokio::task::spawn_blocking(move || {
                            let (items, truncated) = settings::list_paths(&cwd, &prefix);
                            let _ = bus.send(bridge::paths_frame(&prefix, &items, truncated));
                        });
                    }
                    UiInput::PickCwd => {
                        let bus = bus.clone();
                        let picked = tokio::task::spawn_blocking(settings::pick_directory).await;
                        match picked {
                            Ok(Ok(Some(p))) => {
                                let p = std::fs::canonicalize(&p).unwrap_or(p);
                                if !p.is_dir() {
                                    fail(&bus, &format!("目录不可用:{}", p.display()));
                                } else {
                                    if let Some(a) = current(&agent).as_ref() {
                                        a.send(AgentCmd::SetCwd(p.clone()));
                                    }
                                    cwd = p.clone();
                                    live.lock().unwrap().cwd = Some(cwd.display().to_string());
                                    settings::save_last_cwd(&cwd);
                                    let _ = bus.send(bridge::cwd_frame(&cwd.display().to_string()));
                                    send_settings(&bus, &cwd, &cfg, &status, &live);
                                    let _ = bus.send(bridge::command_frame(
                                        "notice",
                                        &format!("工作目录已切换到 {}", cwd.display()),
                                    ));
                                }
                            }
                            Ok(Ok(None)) => {}
                            Ok(Err(e)) => {
                                let _ = bus.send(bridge::cwd_pick_failed_frame(&e));
                            }
                            Err(e) => fail(&bus, &format!("目录选择器失败:{e}")),
                        }
                    }
                    UiInput::NewSession => match current(&agent).as_ref() {
                        Some(a) => {
                            a.send(AgentCmd::NewSession);
                            let _ = bus.send(bridge::command_frame("notice", "已新建会话"));
                        }
                        None => fail(&bus, NO_ENGINE),
                    },
                    UiInput::Clear => match current(&agent).as_ref() {
                        Some(a) => {
                            a.send(AgentCmd::ClearContext);
                        }
                        None => fail(&bus, NO_ENGINE),
                    },
                    UiInput::SetMode(raw) => match Mode::parse(&raw) {
                        Ok(m) => {
                            if let Some(a) = current(&agent).as_ref() {
                                a.send(AgentCmd::SetMode(m));
                            }
                            {
                                let mut s = status.lock().unwrap();
                                s.mode = mode_key(m).to_string();
                                s.mode_label = m.label().to_string();
                            }
                            publish_status(&bus, &live, &status);
                            let _ = bus.send(bridge::command_frame(
                                "notice",
                                &format!("权限已切换:{}", m.hint()),
                            ));
                        }
                        Err(e) => fail(&bus, &format!("{e}")),
                    },
                    UiInput::SetPersona(raw) => {
                        let name = if raw.trim().is_empty() || raw.trim() == "无" {
                            String::new()
                        } else {
                            raw
                        };
                        match current(&agent).as_ref() {
                            Some(a) => {
                                a.send(AgentCmd::SetPersona(name.clone()));
                            }
                            None => fail(&bus, NO_ENGINE),
                        }
                        status.lock().unwrap().persona = name;
                        publish_status(&bus, &live, &status);
                    }
                    UiInput::SetProvider(name) => {
                        match cfg.resolve(None, None, None, Some(name.clone())) {
                            Ok(mut r) => {
                                r.proxy = proxy.clone();
                                let (headers, _) =
                                    cfg.resolve_headers(&r.provider_name, &Default::default());
                                r.headers = headers;
                                let model = r.model.clone();
                                let provider = r.provider_name.clone();
                                let base_url = r.base_url.clone();
                                let api_key_set = r.api_key.is_some();
                                let window = r.effective_context_window();
                                match current(&agent).as_ref() {
                                    Some(a) => {
                                        if !a.send(AgentCmd::Reconfigure(r)) {
                                            fail(&bus, "引擎已退出,配置没有切换。");
                                        }
                                    }
                                    None => fail(&bus, NO_ENGINE),
                                }
                                {
                                    let mut s = status.lock().unwrap();
                                    s.model = model.clone();
                                    s.provider = provider.clone();
                                    s.base_url = base_url;
                                    s.api_key_set = api_key_set;
                                    s.ctx_window = window;
                                }
                                publish_status(&bus, &live, &status);
                                let _ = bus.send(bridge::command_frame(
                                    "notice",
                                    &format!("已切换到 {provider} / {model}(本次运行生效)"),
                                ));
                            }
                            Err(e) => fail(&bus, &format!("切换失败:{e}")),
                        }
                    }
                    UiInput::Compact => match current(&agent).as_ref() {
                        Some(a) => {
                            let _ = bus.send(bridge::command_frame("notice", "▶ 请求压缩上下文…"));
                            a.send(AgentCmd::Compact);
                        }
                        None => fail(&bus, NO_ENGINE),
                    },
                    UiInput::UndoList => {
                        let items: Vec<bridge::UndoFrame> = znaide_core::undo::list()
                            .iter()
                            .enumerate()
                            .map(|(i, s)| bridge::UndoFrame {
                                index: i + 1,
                                session: s.session.clone(),
                                orig: s.orig.clone(),
                                action: s.action.clone(),
                                ts_ms: s.ts_ms,
                                external: s.is_external(),
                            })
                            .collect();
                        let _ = bus.send(bridge::undo_frame(&items));
                    }
                    UiInput::Undo(index) => match znaide_core::undo::rollback_by_index(index) {
                        Ok(snap) => {
                            let _ = bus.send(bridge::command_frame(
                                "notice",
                                &format!("已回滚 #{index}: {} ← 快照 {}", snap.orig, snap.ts_ms),
                            ));
                        }
                        Err(e) => fail(&bus, &format!("回滚失败:{e}")),
                    },
                    UiInput::ResumeSession(id) => match find_session(&id) {
                        Some(path) => match current(&agent).as_ref() {
                            Some(a) => {
                                a.send(AgentCmd::LoadHistory(path));
                                live.lock().unwrap().session_id = Some(id.clone());
                                let _ = bus.send(bridge::session_info_frame(&id));
                            }
                            None => fail(&bus, NO_ENGINE),
                        },
                        None => fail(&bus, &format!("未找到会话:{id}")),
                    },
                    UiInput::DeleteSession(id) => {
                        let current = live.lock().unwrap().session_id.clone();
                        if current.as_deref() == Some(id.as_str()) {
                            fail(&bus, "不能删除当前正在使用的会话。");
                        } else {
                            match find_session(&id) {
                                Some(path) => match znaide_core::session::remove_session(&path) {
                                    Ok(()) => {
                                        refresh_sessions(&bus, &live);
                                        let _ = bus.send(bridge::command_frame(
                                            "notice",
                                            &format!("已删除历史会话:{id}"),
                                        ));
                                    }
                                    Err(e) => fail(&bus, &format!("删除失败:{e}")),
                                },
                                None => fail(&bus, &format!("未找到会话:{id}")),
                            }
                        }
                    }
                    UiInput::RenameSession { id, note } => match find_session(&id) {
                        Some(path) => match znaide_core::session::write_note(&path, &note) {
                            Ok(()) => refresh_sessions(&bus, &live),
                            Err(e) => fail(&bus, &format!("备注保存失败:{e}")),
                        },
                        None => fail(&bus, &format!("未找到会话:{id}")),
                    },
                    UiInput::OpenSettings => {
                        // 配置可能被 TUI/手工改过:打开设置页时重读一次(读坏了就沿用内存里的)
                        if let Ok(fresh) = Config::load() {
                            cfg = fresh;
                        }
                        send_settings(&bus, &cwd, &cfg, &status, &live)
                    }
                    UiInput::ProbeModels { base_url, api_key } => {
                        let bus = bus.clone();
                        let proxy = proxy.clone();
                        let provider = current_provider(&cfg, &status);
                        let key = if api_key.trim().is_empty() {
                            cfg.resolve(None, None, None, Some(provider.clone()))
                                .ok()
                                .and_then(|r| r.api_key)
                        } else {
                            Some(api_key.trim().to_string())
                        };
                        let headers = cfg.resolve_headers(&provider, &Default::default()).0;
                        tokio::spawn(async move {
                            match znaide_core::llm::openai::probe_models(
                                &base_url,
                                key.as_deref(),
                                &proxy,
                                &headers,
                            )
                            .await
                            {
                                Ok(models) => {
                                    let _ = bus.send(bridge::model_list_frame(&models));
                                }
                                Err(e) => {
                                    let _ =
                                        bus.send(bridge::error_frame(format!("探测模型失败:{e}")));
                                }
                            }
                        });
                    }
                    UiInput::SaveModel(form) => {
                        let provider = form.provider.trim().to_string();
                        if provider.is_empty() || form.model.trim().is_empty() {
                            fail(&bus, "服务商与模型都不能为空。");
                            continue;
                        }
                        let was_configured = status.lock().unwrap().configured;
                        let mut draft = cfg.clone();
                        draft.max_turns = form.max_turns;
                        let key = if form.clear_key {
                            Some("")
                        } else if form.api_key.trim().is_empty() {
                            None
                        } else {
                            Some(form.api_key.trim())
                        };
                        let window = if form.context_window == 0 {
                            None
                        } else {
                            Some(form.context_window)
                        };
                        if let Err(e) = draft.save(
                            &provider,
                            Some(form.model.trim()),
                            Some(form.base_url.trim()),
                            key,
                            window,
                        ) {
                            fail(&bus, &format!("保存失败:{e}"));
                            continue;
                        }
                        cfg = draft;
                        match cfg.resolve(None, None, None, Some(provider.clone())) {
                            Ok(mut r) => {
                                r.headers =
                                    cfg.resolve_headers(&r.provider_name, &Default::default()).0;
                                let model = r.model.clone();
                                let prov = r.provider_name.clone();
                                let base_url = r.base_url.clone();
                                let api_key_set = r.api_key.is_some();
                                let window = r.effective_context_window();
                                if let Some(a) = current(&agent).as_ref() {
                                    a.send(AgentCmd::Reconfigure(r));
                                } else {
                                    // 首启向导:配置刚变可用,补建引擎(事件通道启动时就建好了)
                                    let persona = status.lock().unwrap().persona.clone();
                                    match znaide_host::agent::spawn(
                                        AgentConfig {
                                            resolved: r,
                                            cwd: cwd.clone(),
                                            mode,
                                            persona,
                                            max_turns,
                                            retry,
                                            resume_session_id: None,
                                        },
                                        ev_tx.clone(),
                                    ) {
                                        Ok(h) => {
                                            *agent.lock().unwrap() = Some(Arc::new(h));
                                            let _ = bus.send(bridge::configured_frame());
                                        }
                                        Err(e) => {
                                            fail(&bus, &format!("引擎启动失败:{e}"));
                                        }
                                    }
                                }
                                {
                                    let mut s = status.lock().unwrap();
                                    s.model = model;
                                    s.provider = prov;
                                    s.base_url = base_url;
                                    s.api_key_set = api_key_set;
                                    s.configured = true;
                                    s.ctx_window = window;
                                }
                                publish_status(&bus, &live, &status);
                                let _ = bus.send(bridge::command_frame(
                                    "notice",
                                    "✔ 模型配置已保存并生效",
                                ));
                                // 首启向导:配置完成后让页面回到对话
                                if !was_configured {
                                    let _ = bus.send(bridge::configured_frame());
                                }
                            }
                            Err(e) => fail(&bus, &format!("已保存,但解析失败:{e}")),
                        }
                        send_settings(&bus, &cwd, &cfg, &status, &live);
                    }
                    UiInput::SaveNetwork(form) => {
                        let Some(mode) = ProxyMode::parse(&form.mode) else {
                            fail(&bus, "未知代理档位。");
                            continue;
                        };
                        let proxy = ProxyConfig {
                            mode,
                            url: if form.url.trim().is_empty() {
                                None
                            } else {
                                Some(form.url.trim().to_string())
                            },
                            warning: None,
                        };
                        let headers: Vec<(String, String)> = form
                            .headers
                            .iter()
                            .map(|h| (h.name.clone(), h.value.clone()))
                            .collect();
                        let provider_headers: Vec<(String, String)> = form
                            .provider_headers
                            .iter()
                            .map(|h| (h.name.clone(), h.value.clone()))
                            .collect();
                        let mut draft = cfg.clone();
                        let max_turns = draft.max_turns;
                        let provider_now = current_provider(&cfg, &status);
                        match draft.save_runtime_settings(
                            max_turns,
                            Some(form.retry),
                            proxy,
                            form.headers_enabled,
                            headers,
                        ) {
                            Ok(mut notes) => {
                                match draft.save_provider_headers(
                                    &provider_now,
                                    form.provider_headers_enabled,
                                    provider_headers,
                                ) {
                                    Ok(more) => notes.extend(more),
                                    Err(e) => fail(&bus, &format!("服务商请求头保存失败:{e}")),
                                }
                                cfg = draft;
                                for n in &notes {
                                    let _ = bus
                                        .send(bridge::command_frame("notice", &format!("⚠ {n}")));
                                }
                                // 轮数/重试立即生效;代理要重启(宿主重配不换代理)
                                let provider = current_provider(&cfg, &status);
                                if let Ok(mut r) = cfg.resolve(None, None, None, Some(provider)) {
                                    r.headers = cfg
                                        .resolve_headers(&r.provider_name, &Default::default())
                                        .0;
                                    if let Some(a) = current(&agent).as_ref() {
                                        a.send(AgentCmd::Reconfigure(r));
                                    }
                                }
                                let _ = bus.send(bridge::command_frame(
                                    "notice",
                                    "✔ 网络与请求设置已保存(代理改动重启后生效)",
                                ));
                                send_settings(&bus, &cwd, &cfg, &status, &live);
                            }
                            Err(e) => fail(&bus, &format!("保存失败:{e}")),
                        }
                    }
                    UiInput::SavePersonaFile { name, body } => {
                        match settings::save_persona(&name, &body) {
                            Ok(path) => {
                                let items = znaide_core::persona::list();
                                let _ = bus.send(bridge::personas_frame(&items));
                                let _ = bus.send(bridge::command_frame(
                                    "notice",
                                    &format!("✔ 人格已保存:{}", path.display()),
                                ));
                                send_settings(&bus, &cwd, &cfg, &status, &live);
                            }
                            Err(e) => fail(&bus, &format!("保存人格失败:{e}")),
                        }
                    }
                    UiInput::SkillGet(name) => match settings::read_skill(&cwd, &name) {
                        Ok((path, body, editable)) => {
                            let _ = bus.send(bridge::skill_content_frame(
                                &name,
                                &path.display().to_string(),
                                &body,
                                editable,
                            ));
                        }
                        Err(e) => fail(&bus, &format!("{e}")),
                    },
                    UiInput::SkillSave { name, body } => {
                        match settings::save_skill(&cwd, &name, &body) {
                            Ok(path) => {
                                let _ = bus.send(bridge::command_frame(
                                    "notice",
                                    &format!("✔ 技能已保存:{}", path.display()),
                                ));
                                send_settings(&bus, &cwd, &cfg, &status, &live);
                            }
                            Err(e) => fail(&bus, &format!("保存技能失败:{e}")),
                        }
                    }
                    UiInput::SkillRun { name, args } => match current(&agent).as_ref() {
                        Some(a) => {
                            let _ = bus.send(bridge::command_frame(
                                "notice",
                                &format!("▶ 运行技能「{name}」…"),
                            ));
                            a.send(AgentCmd::RunSkill { name, args });
                        }
                        None => fail(&bus, NO_ENGINE),
                    },
                    UiInput::MemoryGet { filename, scope } => {
                        match settings::read_memory(&cwd, &filename, &scope) {
                            Ok(body) => {
                                let meta = settings::memory_meta(&cwd, &filename, &scope);
                                let _ = bus.send(bridge::memory_content_frame(
                                    &filename,
                                    &scope,
                                    meta.as_ref().map(|m| m.name.as_str()).unwrap_or(""),
                                    meta.as_ref().map(|m| m.description.as_str()).unwrap_or(""),
                                    meta.as_ref().map(|m| m.mtype.as_str()).unwrap_or(""),
                                    &body,
                                ));
                            }
                            Err(e) => fail(&bus, &format!("读取记忆失败:{e}")),
                        }
                    }
                    UiInput::MemorySave(form) => match settings::save_memory(&cwd, &form) {
                        Ok(path) => {
                            let _ = bus.send(bridge::command_frame(
                                "notice",
                                &format!("✔ 记忆已保存:{}", path.display()),
                            ));
                            send_settings(&bus, &cwd, &cfg, &status, &live);
                        }
                        Err(e) => fail(&bus, &format!("保存记忆失败:{e}")),
                    },
                    UiInput::MemoryDelete { filename, scope } => {
                        let Some(scope_kind) = settings::parse_scope(&scope) else {
                            fail(&bus, "未知作用域。");
                            continue;
                        };
                        match znaide_core::tools::memory::delete_memory(&filename, scope_kind, &cwd)
                        {
                            Ok(true) => {
                                let _ = bus.send(bridge::command_frame(
                                    "notice",
                                    &format!("🗑 已删除记忆:{filename}"),
                                ));
                                send_settings(&bus, &cwd, &cfg, &status, &live);
                            }
                            Ok(false) => fail(&bus, &format!("记忆不存在:{filename}")),
                            Err(e) => fail(&bus, &format!("删除记忆失败:{e}")),
                        }
                    }
                    UiInput::McpSave(text) => match settings::save_mcp(&text) {
                        Ok(path) => {
                            let _ = bus.send(bridge::command_frame(
                                "notice",
                                &format!(
                                    "✔ mcp.json 已保存:{}\n改动在下次启动时连接。",
                                    path.display()
                                ),
                            ));
                            send_settings(&bus, &cwd, &cfg, &status, &live);
                        }
                        Err(e) => fail(&bus, &format!("保存 mcp.json 失败:{e}")),
                    },
                    UiInput::UpdateCheck => {
                        let bus = bus.clone();
                        let proxy = proxy.clone();
                        tokio::spawn(async move {
                            let text = match znaide_core::update::http_client(&proxy) {
                                Ok(client) => {
                                    match znaide_core::update::probe_latest_prefixed(
                                        &client, "gui-v",
                                    )
                                    .await
                                    {
                                        Ok((src, latest)) => {
                                            let cur = GUI_VERSION;
                                            if znaide_core::update::version_gt(&latest, &cur) {
                                                format!(
                                                    "发现新版本 v{latest}(来源 {})\n当前 v{cur}:点『立即更新』升级。",
                                                    src.label()
                                                )
                                            } else {
                                                format!("已是最新版本 v{cur}。")
                                            }
                                        }
                                        Err(e) => format!("检查失败:{e}"),
                                    }
                                }
                                Err(e) => format!("检查失败:{e}"),
                            };
                            let _ = bus.send(bridge::update_result_frame("check", &text));
                        });
                    }
                    UiInput::UpdateInstall => {
                        let bus = bus.clone();
                        let proxy = proxy.clone();
                        tokio::spawn(async move {
                            let text = znaide_host::update::install_for_version(
                                "xiufan",
                                "gui-v",
                                GUI_VERSION,
                                &proxy,
                            )
                            .await;
                            let _ = bus.send(bridge::update_result_frame("install", &text));
                        });
                    }
                    UiInput::Permission { allow, always } => match current(&agent).as_ref() {
                        Some(a) => {
                            if !a.answer_permission(allow, always) {
                                fail(&bus, "没有待回答的权限询问(可能已在别处回答)。");
                            }
                        }
                        None => fail(&bus, NO_ENGINE),
                    },
                    UiInput::Rounds { cont } => match current(&agent).as_ref() {
                        Some(a) => {
                            if !a.answer_rounds(cont) {
                                fail(&bus, "没有待回答的轮数询问(可能已在别处回答)。");
                            }
                        }
                        None => fail(&bus, NO_ENGINE),
                    },
                }
            }
        }
    });

    let token = server::new_token();
    let hello = Hello {
        version: GUI_VERSION.to_string(),
        product: PRODUCT.to_string(),
        gui_version: GUI_VERSION.to_string(),
        engine_version: ENGINE_VERSION.to_string(),
        model: prepared
            .resolved
            .as_ref()
            .map(|r| r.model.clone())
            .unwrap_or_default(),
        provider: prepared
            .resolved
            .as_ref()
            .map(|r| r.provider_name.clone())
            .unwrap_or_default(),
        cwd: prepared.cwd.display().to_string(),
        mode: mode_key(mode).to_string(),
        mode_label: mode.label().to_string(),
        persona: prepared.persona.clone(),
        first_run: prepared.first_run,
        resolve_error: prepared.resolve_error.clone(),
        ctx_window: prepared
            .resolved
            .as_ref()
            .and_then(|r| r.effective_context_window()),
        resume: prepared
            .resume
            .as_ref()
            .and_then(|p| p.file_stem())
            .map(|s| s.to_string_lossy().to_string()),
        notes: prepared.notes.iter().map(Into::into).collect(),
        providers: prepared
            .cfg
            .list_providers()
            .into_iter()
            .map(|(name, _base, model)| bridge::ProviderFrame { name, model })
            .collect(),
        personas: znaide_core::persona::list(),
        help: znaide_host::command::HELP_TEXT.to_string(),
        skills: znaide_core::skills::scan(&prepared.cwd)
            .skills
            .into_iter()
            .map(|s| bridge::SkillFrame {
                name: s.name,
                desc: s.description,
            })
            .collect(),
    };

    let token_for_state = token.clone();
    let (addr, _srv) = server::serve(cli.port, move |port| {
        Arc::new(AppState {
            ui: ui_tx.clone(),
            events: bus.clone(),
            token: token_for_state,
            port,
            hello: hello.clone(),
            live: live.clone(),
            clients: Arc::default(),
        })
    })
    .await?;

    let url = format!("http://{addr}/?token={token}");
    diag::log(format!("服务已监听:{addr} cwd={}", prepared.cwd.display()));
    diag::say(format!("{} 已启动", full_title()));
    diag::say(format!("  构建:{BUILD_STAMP}"));
    diag::say(format!("  地址:{url}"));
    diag::say("  (token 是本次启动的一次性口令;关掉窗口/进程即失效)");
    if let Some(e) = &prepared.resolve_error {
        diag::warn(format!("⚠ 配置未就绪:{e}"));
    }

    Ok(Some(Running {
        url,
        agent,
        shutdown,
    }))
}

/// 启动完成后交给窗口/浏览器模式的句柄。运行时必须活到窗口关闭。
struct Running {
    url: String,
    agent: Arc<Mutex<Option<Arc<AgentHandle>>>>,
    shutdown: Arc<tokio::sync::Notify>,
}

/// 单实例锁:`<data>/gui.lock` 上一把排他文件锁;崩溃/退出时内核自动释放。
fn acquire_instance_lock() -> anyhow::Result<std::fs::File> {
    use std::io::Write;
    let dir = znaide_core::config::data_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("gui.lock");
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)?;
    if fs2::FileExt::try_lock_exclusive(&f).is_err() {
        anyhow::bail!(
            "znaide-gui 已在运行(数据目录 {})。\n要开第二个,请先关掉正在运行的那个。",
            dir.display()
        );
    }
    f.set_len(0)?;
    write!(f, "{}", std::process::id())?;
    Ok(f)
}

/// 取当前引擎(没有就 None)。克隆出 Arc,避免把锁 guard 带过 await。
fn current(slot: &Arc<Mutex<Option<Arc<AgentHandle>>>>) -> Option<Arc<AgentHandle>> {
    slot.lock().unwrap().clone()
}

/// 设置页快照(保存后重发,前端整页重填)。
fn send_settings(
    bus: &broadcast::Sender<String>,
    cwd: &std::path::Path,
    cfg: &Config,
    status: &Arc<Mutex<bridge::Status>>,
    live: &Arc<Mutex<LiveState>>,
) {
    let notice = live.lock().unwrap().update_notice.clone();
    let data = settings::snapshot(cwd, cfg, &status.lock().unwrap(), notice.as_deref());
    let _ = bus.send(bridge::settings_frame(data));
}

/// 当前生效的服务商(运行中状态优先,其次配置)。
fn current_provider(cfg: &Config, status: &Arc<Mutex<bridge::Status>>) -> String {
    let p = status.lock().unwrap().provider.clone();
    if p.is_empty() {
        cfg.provider.clone().unwrap_or_else(|| "ollama".into())
    } else {
        p
    }
}

/// 侧边栏刷新:扫一遍会话文件 → 记住并广播。写盘之后调用(读文件,放阻塞线程池)。
fn refresh_sessions(bus: &broadcast::Sender<String>, live: &Arc<Mutex<LiveState>>) {
    let items = bridge::session_frames();
    live.lock().unwrap().sessions = items.clone();
    let _ = bus.send(bridge::sessions_frame(&items));
}

/// 按会话 id(文件名 stem)定位历史文件。
fn find_session(id: &str) -> Option<PathBuf> {
    znaide_core::session::list_history_sessions_detailed()
        .into_iter()
        .find(|h| {
            h.path
                .file_stem()
                .map(|s| s.to_string_lossy() == id)
                .unwrap_or(false)
        })
        .map(|h| h.path)
}

/// 顶栏状态变化:记住(重连补发)并广播给所有页面。
fn publish_status(
    bus: &broadcast::Sender<String>,
    live: &Arc<Mutex<LiveState>>,
    status: &Arc<Mutex<bridge::Status>>,
) {
    let s = status.lock().unwrap().clone();
    live.lock().unwrap().status = Some(s.clone());
    let _ = bus.send(bridge::status_frame(&s));
}

fn mode_key(mode: Mode) -> &'static str {
    match mode {
        Mode::Ask => "ask",
        Mode::AcceptEdits => "acceptEdits",
        Mode::BypassPermissions => "bypassPermissions",
        Mode::Yolo => "yolo",
    }
}

/// 是否已有可用配置来源(与 CLI 共用同一条判定,避免"命令行给齐了却还当首启")
fn config_provided(cli: &Cli) -> bool {
    znaide_core::config::config_exists()
        || cli.model.is_some()
        || cli.base_url.is_some()
        || cli.api_key.is_some()
        || cli.provider.is_some()
        || znaide_core::config::env_configured()
}

/// 用系统默认浏览器打开页面;失败只提示,不影响服务。
fn open_browser(url: &str) {
    let (cmd, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(target_os = "windows") {
        ("cmd", vec!["/C", "start", "", url])
    } else {
        ("xdg-open", vec![url])
    };
    let mut child = std::process::Command::new(cmd);
    child.args(args);
    // GUI 无控制台:cmd /C start 也会闪一下黑框
    znaide_core::winproc::hide_console_window(&mut child);
    if let Err(e) = child.spawn() {
        diag::warn(format!("⚠ 没能自动打开浏览器({e});请手动访问上面的地址。"));
    }
}
