//! 会话驱动:动作面 + 后台任务。
//!
//! 前端(TUI / GUI)只做两件事:把"键盘/按钮"翻译成 [`AgentCmd`],以及消费
//! `SessionEvent`。任务本身拥有 `Session`,所以两个客户端共用同一套回合语义、
//! 取消语义与投递箱行为 —— 不存在"TUI 一种暂停方式、GUI 另一种"。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use znaide_core::config::Resolved;
use znaide_core::llm::OpenAiClient;
use znaide_core::permissions::Mode;
use znaide_core::session::{Session, SessionEvent, SharedInbox};

/// 发送给 agent 的命令(动作面)。语义与 TUI 的 slash 命令/按钮一一对应。
#[derive(Debug)]
pub enum AgentCmd {
    Prompt(String),
    SetMode(Mode),
    /// 从历史文件恢复会话
    LoadHistory(PathBuf),
    /// 运行时重配置(切换 provider/模型/端点/key,立即生效)
    Reconfigure(Resolved),
    /// 手动触发技能:/技能名 [参数](可能含入口脚本,由 agent 侧统一执行)
    RunSkill { name: String, args: String },
    /// 清空会话上下文与历史文件(经确认后发送)
    ClearContext,
    /// 压缩上下文(旧消息 → 摘要,腾出上下文空间)
    Compact,
    /// 切换全局人格(/persona <名字>;空/无 = 关闭),写回配置持久生效
    SetPersona(String),
}

/// 共享取消槽:前端随时可取消"当前回合"。
pub type SharedCancel = Arc<Mutex<Option<CancellationToken>>>;

/// 起一个会话任务所需的全部输入(与界面无关)。
pub struct AgentConfig {
    pub resolved: Resolved,
    pub cwd: PathBuf,
    pub mode: Mode,
    pub persona: String,
    pub max_turns: Option<usize>,
    pub retry: Option<usize>,
    pub no_retry: bool,
    /// `--resume` 的会话 ID(沿用该历史文件的身份;None = 新建)
    pub resume_session_id: Option<String>,
}

/// 会话任务句柄。前端**不**直接持有 `Session`,也不自己 spawn 任务。
pub struct AgentHandle {
    cmd_tx: UnboundedSender<AgentCmd>,
    cancel: SharedCancel,
    inbox: SharedInbox,
    task: JoinHandle<()>,
}

impl AgentHandle {
    /// 发一个动作;返回 false 表示任务已退出(前端可据此提示"引擎已退出")
    pub fn send(&self, cmd: AgentCmd) -> bool {
        self.cmd_tx.send(cmd).is_ok()
    }

    /// 需要多处持有时用它克隆发送端(不暴露内部字段)
    pub fn cmd_tx(&self) -> UnboundedSender<AgentCmd> {
        self.cmd_tx.clone()
    }

    /// Esc:取消当前回合(没有进行中的回合时是空操作)
    pub fn cancel_round(&self) {
        if let Some(tok) = self.cancel.lock().unwrap().as_ref() {
            tok.cancel();
        }
    }

    /// 运行中插话的投递箱:前端把它交给自己的输入队列,引擎在轮边界取走
    pub fn inbox(&self) -> SharedInbox {
        self.inbox.clone()
    }

    /// 退出:停掉后台任务(未落盘的回合被丢弃,与旧行为一致)
    pub fn shutdown(&self) {
        self.task.abort();
    }
}

/// 起会话任务。`ev_tx` 是前端的事件出口 —— `SessionEvent` **原样转发**,
/// 不在这里重新包装(事件面是 core 的契约,GUI 也吃同一份)。
pub fn spawn(cfg: AgentConfig, ev_tx: UnboundedSender<SessionEvent>) -> Result<AgentHandle> {
    let llm = OpenAiClient::new(&cfg.resolved)?;
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<AgentCmd>();
    let cancel: SharedCancel = Arc::default();
    let inbox: SharedInbox = Arc::default();
    let cancel_agent = cancel.clone();
    let inbox_agent = inbox.clone();
    let AgentConfig {
        cwd,
        mode,
        persona,
        max_turns,
        retry,
        no_retry,
        resume_session_id,
        ..
    } = cfg;

    let task = tokio::spawn(async move {
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
            cwd,
            mode,
            Some(ev_tx.clone()),
            cancel.clone(),
            true,
            resume_session_id,
            mcp,
            &persona,
        ) {
            Ok(s) => s,
            Err(_) => return,
        };
        // 轮数上限:--max-turns > config 的 max_turns(0 = 不限)> 默认。
        session.set_max_turns(crate::policy::max_turns(max_turns));
        // 弱网重试:--retry / --no-retry > config 的 retry > 0(关,旧行为)
        session.set_retry(crate::policy::retry(retry, no_retry));
        // 运行中插话:引擎在每个轮边界从这个投递箱取一条插进对话
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
                        *cancel_agent.lock().unwrap() = Some(round_cancel.clone());
                        session.reset_cancel(round_cancel);
                        let _ = session.run_turn(&p).await;
                        // 回合结束,清空槽,避免误取消下一回合
                        *cancel_agent.lock().unwrap() = None;
                    }
                    Some(AgentCmd::SetMode(m)) => {
                        session.set_mode(m);
                    }
                    Some(AgentCmd::RunSkill { name, args }) => {
                        let round_cancel = CancellationToken::new();
                        *cancel_agent.lock().unwrap() = Some(round_cancel.clone());
                        session.reset_cancel(round_cancel);
                        let _ = session.run_skill_turn(&name, &args).await;
                        *cancel_agent.lock().unwrap() = None;
                    }
                    Some(AgentCmd::Reconfigure(r)) => {
                        // 代理不跟着模型重配走:界面这一轮不提供代理入口,拿当前生效那份
                        // 顶上,免得一次 /config 就把 --proxy 或配置文件里的代理冲掉。
                        let mut r = r;
                        r.proxy = session.proxy().clone();
                        if let Err(e) = session.reconfigure(&r) {
                            session.notify(format!("⚠ 重配失败(仍用原连接):{e}"));
                        }
                        // 轮数上限与重试次数也跟着重读(/config 改了立即生效;
                        // 命令行给了覆盖的话仍以命令行优先)
                        session.set_max_turns(crate::policy::max_turns(max_turns));
                        session.set_retry(crate::policy::retry(retry, no_retry));
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

    Ok(AgentHandle {
        cmd_tx,
        cancel,
        inbox,
        task,
    })
}
