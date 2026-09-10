use crate::config::data_dir;
use crate::llm::openai::{AssistantReply, StreamEvent};
use crate::llm::types::{ChatMessage, ToolCall, ToolDef};
use crate::llm::OpenAiClient;
use crate::permissions::{inspect_command, CommandRisk, Mode, Permission};
use crate::refs::expand_at_refs;
use crate::tools::{self, ToolContext};
use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// 需要权限的动作类别
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermKind {
    Write,
    Command,
}

/// 事件:引擎 → UI(经 channel)
#[derive(Debug)]
pub enum SessionEvent {
    TurnStarted,
    /// 思考内容增量
    ReasoningDelta(String),
    /// 正文增量
    TextDelta(String),
    /// 工具开始执行
    ToolStarted {
        name: String,
        args: String,
        /// 命令类工具的生效静默预算(毫秒);其它工具无此概念为 None。
        /// UI 拿它给工具卡片标注"静默上限"
        idle_ms: Option<u64>,
    },
    /// 工具执行结束
    ToolFinished { name: String, ok: bool, output: String },
    /// 命令长时间无输出、接近被静默超时终止:UI 让对应卡片变黄(1)/红(2);
    /// level=0 表示恢复输出、解除预警
    ToolSilentAlert { name: String, level: u8 },
    /// 命令实时输出增量(执行中推给 UI 滚动显示;约 200ms 节流一次;
    /// 无头模式 events=None 不发)
    ToolOutputDelta { name: String, delta: String },
    /// 全局人格已切换(空 = 关闭):UI 据此更新状态栏人格显示
    PersonaChanged { name: String },
    /// 权限询问(交互模式):UI 用 tx 回 (是否允许, 本会话是否总是允许)
    PermissionRequest {
        kind: PermKind,
        title: String,
        body: String,
        /// Some(说明) = 高危命令或目标无法判定:UI 用红色渲染该说明,且不提供
        /// "本会话都允许"(只允许单次放行)
        warning: Option<String>,
        tx: oneshot::Sender<(bool, bool)>,
    },
    /// 一轮模型往返开始(轮次预算用):used = 已用轮数(含本轮),limit = 上限
    /// (0 = 不限)。UI 拿它显示"轮 x/y"。
    RoundStarted { used: usize, limit: usize },
    /// 轮次预算用尽、任务还没完(交互模式):问 UI 要不要再放一批。
    /// tx 回 true = 再放一批(默认 200 轮),false / Esc / 通道关闭 = 就此收尾
    RoundsExhausted { used: usize, tx: oneshot::Sender<bool> },
    /// 通知/提示(如危险命令拦截)
    Notice(String),
    /// 一次模型调用的真实 token 用量(端点不提供 usage 就不发)
    Usage { prompt: u64, completion: u64 },
    /// 上下文与历史已清空(/clear):UI 记得同步清掉展示
    ContextCleared,
    /// 压缩开始(/compact 耗时长,UI 先显示进行中)
    CompactionStarted,
    /// 上下文已压缩:旧消息折成一段摘要,只留摘要 + 最近几条;UI 据此重建
    ContextCompacted {
        /// 被摘要掉的旧消息条数
        removed: usize,
        /// 生成的中文摘要
        summary: String,
        /// 压缩后保留的最近消息(UI 重建用)
        kept: Vec<ChatMessage>,
    },
    /// 回合结束
    TurnFinished { text: String, truncated: bool },
    /// 会话元信息(会话 id、历史文件路径等)
    SessionInfo { id: String, history: Option<String> },
    /// 历史会话载入成功(消息列表给 UI 恢复展示)
    HistoryLoaded { count: usize, messages: Vec<ChatMessage> },
}

/// 回合执行结果(供 headless 使用)
pub struct TurnResult {
    pub text: String,
    pub tool_calls: usize,
    pub truncated: bool,
    /// 本回合输入 token 累计(端点不给 usage 则为 0)
    pub input_tokens: u64,
    /// 本回合输出 token 累计(端点不给 usage 则为 0)
    pub output_tokens: u64,
}

/// 单条消息默认允许的模型往返轮数(一轮可含多次工具调用)。可用
/// `--max-turns` / config 的 `max_turns` 覆盖;0 = 不限。
pub const DEFAULT_MAX_TURNS: usize = 200;

/// 同一次(工具, 参数)完全相同的调用累计到这个次数:在工具结果里插一句提醒,
/// 劝模型换方法——不死板地掐断,先给一次自救机会。
const REPEAT_WARN: usize = 3;
/// 相同调用累计到这个次数还没换路:判定原地打转,收尾。
const REPEAT_ABORT: usize = 6;

/// 会话:多轮对话 + 工具循环。交互模式把事件发给 UI,无头模式按规则自动放行/拒绝;
/// persist=true 时消息记到 ~/.znaide/sessions/<id>.jsonl;写文件前自动做 undo 快照。
pub struct Session {
    llm: OpenAiClient,
    cwd: PathBuf,
    session_id: String,
    mode: Mode,
    events: Option<mpsc::UnboundedSender<SessionEvent>>,
    cancel: CancellationToken,
    file_always: bool,
    command_always: bool,
    messages: Vec<ChatMessage>,
    max_turns: usize,
    history_writer: Option<std::fs::File>,
    history_path: Option<PathBuf>,
    /// MCP 工具管理器(可为空)
    mcp: crate::mcp::McpManager,
    /// 全局人格(空 = 不注入,见 persona 模块);切换会写回 config 持久
    persona: String,
}

impl Session {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        llm: OpenAiClient,
        cwd: PathBuf,
        mode: Mode,
        events: Option<mpsc::UnboundedSender<SessionEvent>>,
        cancel: CancellationToken,
        persist: bool,
        session_id: Option<String>,
        mcp: Option<crate::mcp::McpManager>,
        persona: &str,
    ) -> anyhow::Result<Self> {
        let id = session_id.unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| format!("{:x}", d.as_millis()))
                .unwrap_or_else(|_| "s".into())
        });
        // 懒创建:先只登记路径,首次 record 时才落盘——
        // 启动后没对话就退出的会话不再留下 0 字节空壳文件
        let (history_writer, history_path) = if persist {
            let dir = data_dir().join("sessions");
            std::fs::create_dir_all(&dir)?;
            let path = dir.join(format!("{id}.jsonl"));
            (None, Some(path))
        } else {
            (None, None)
        };

        let mut s = Self {
            llm,
            cwd,
            session_id: id,
            mode,
            events,
            cancel,
            file_always: false,
            command_always: false,
            messages: Vec::new(),
            max_turns: DEFAULT_MAX_TURNS,
            history_writer,
            history_path,
            mcp: mcp.unwrap_or_else(crate::mcp::McpManager::start_empty),
            persona: persona.to_string(),
        };
        s.push_system_prompt();
        s.emit(SessionEvent::SessionInfo {
            id: s.session_id.clone(),
            history: s
                .history_path
                .as_ref()
                .map(|p| p.display().to_string()),
        });
        Ok(s)
    }

    /// MCP 管理器引用
    pub fn mcp(&self) -> &crate::mcp::McpManager {
        &self.mcp
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// 换模型/端点/key,立即生效,不打断会话
    pub fn reconfigure(&mut self, resolved: &crate::config::Resolved) {
        self.llm.reconfigure(resolved);
    }

    /// 设置单条消息的轮数上限(0 = 不限)。CLI `--max-turns` 与 config 的
    /// `max_turns` 都从这里进来;/config 改动后由宿主重新调用即可即时生效。
    pub fn set_max_turns(&mut self, n: usize) {
        self.max_turns = n;
    }

    /// 当前轮数上限(0 = 不限)
    pub fn max_turns(&self) -> usize {
        self.max_turns
    }

    pub fn current_model(&self) -> &str {
        self.llm.model()
    }

    pub fn history_path(&self) -> Option<&Path> {
        self.history_path.as_deref()
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    pub fn reset_cancel(&mut self, cancel: CancellationToken) {
        self.cancel = cancel;
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        self.file_always = false;
        self.command_always = false;
        // 系统提示里含模式描述,切换后重建,让模型感知当前模式
        self.refresh_system_prompt();
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// 清空上下文与历史文件(/clear):消息回到只剩系统提示,jsonl 截断,重新开始
    pub fn clear_context(&mut self) {
        self.messages.clear();
        self.push_system_prompt();
        // 截断历史文件:仅当文件已存在或曾有写入时(懒创建下不凭空建空文件)
        let existed = self.history_writer.is_some()
            || self
                .history_path
                .as_ref()
                .map(|p| p.exists())
                .unwrap_or(false);
        if existed {
            if let Some(path) = self.history_path.clone() {
                self.history_writer = None;
                if let Ok(f) = OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(&path)
                {
                    self.history_writer = Some(f);
                }
            }
        }
        self.emit(SessionEvent::ContextCleared);
    }

    /// 压缩上下文(/compact):把旧消息交给模型折成一段中文摘要,
    /// 上下文变成 system + 摘要 + 最近 4 条,腾出空间。历史 jsonl 不动,
    /// resume 后还能再压。返回被压掉的条数(没得压返回 0)。
    /// 注意:成功才替换 messages,模型调用失败保持原样。
    pub async fn compact_context(&mut self) -> anyhow::Result<usize> {
        const KEEP_TAIL: usize = 4;
        if self.events.is_some() {
            // 压缩挺慢,先让 UI 显示个"压缩中"状态
            self.emit(SessionEvent::CompactionStarted);
        }
        let Some(sys) = self.messages.first().cloned() else {
            self.emit(SessionEvent::TurnFinished { text: String::new(), truncated: false });
            return Ok(0);
        };
        if sys.role != crate::llm::types::Role::System {
            self.emit(SessionEvent::TurnFinished { text: String::new(), truncated: false });
            return Ok(0);
        }
        // 只读拆分,成功前不动 self.messages
        let tail: Vec<ChatMessage> = self.messages.iter().skip(1).cloned().collect();
        if tail.len() <= KEEP_TAIL + 1 {
            // 历史太短,没有可压缩空间
            self.emit(SessionEvent::TurnFinished { text: String::new(), truncated: false });
            return Ok(0);
        }
        let keep_at = tail.len().saturating_sub(KEEP_TAIL);
        let (old, kept) = tail.split_at(keep_at);
        let removed = old.len();
        // 展平为摘要输入(每条截断,避免压缩请求本身超窗)
        let mut body = String::new();
        for m in old {
            let label = match m.role {
                crate::llm::types::Role::System => "系统",
                crate::llm::types::Role::User => "用户",
                crate::llm::types::Role::Assistant => "助手",
                crate::llm::types::Role::Tool => "工具结果",
            };
            let content = m.content.clone().unwrap_or_default();
            let content: String = content.chars().take(2_000).collect();
            body.push_str(&format!("[{label}] {content}\n"));
        }
        let sum_msgs = vec![
            ChatMessage::system(
                "你是会话上下文压缩器。把用户提供的对话压缩成精炼的中文要点列表,必须保留:\n\
                 - 任务目标与当前进展到哪一步\n\
                 - 已完成的关键操作与结果(重要的文件路径、命令、数据结论)\n\
                 - 用户的明确决定、偏好与要求\n\
                 - 尚未完成、需要继续处理的事项\n\
                 只输出要点,不要复述对话原文,不要添加对话里没有的内容。",
            ),
            ChatMessage::user(body),
        ];
        let reply = self.llm.chat(&sum_msgs, None).await;
        let reply = match reply {
            Ok(r) => r,
            Err(e) => {
                // 失败:复位 UI 状态,保持原上下文不动
                self.emit(SessionEvent::TurnFinished { text: String::new(), truncated: false });
                return Err(e);
            }
        };
        let summary = reply.content.unwrap_or_default();
        if summary.trim().is_empty() {
            self.emit(SessionEvent::TurnFinished { text: String::new(), truncated: false });
            anyhow::bail!("压缩模型没返回内容,再试一次");
        }
        let summary_msg = ChatMessage::user(format!(
            "【上下文摘要:压缩自此前 {removed} 条消息,由模型自动生成,细节以历史文件为准】\n{summary}"
        ));
        let mut new_messages = vec![sys];
        new_messages.push(summary_msg);
        new_messages.extend_from_slice(kept);
        // 到这里才算成功,才替换消息
        self.messages = new_messages;
        self.emit(SessionEvent::ContextCompacted {
            removed,
            summary,
            kept: kept.to_vec(),
        });
        Ok(removed)
    }

    /// 从历史 jsonl 恢复会话(替换当前消息,重建系统提示)
    pub fn load_history(&mut self, path: &Path) -> anyhow::Result<usize> {
        let text = std::fs::read_to_string(path)?;
        let mut loaded = 0usize;
        // 首行 meta 里的历史格式版本(老文件没有则无提示)
        let mut meta_schema: Option<String> = None;
        self.messages.clear();
        self.push_system_prompt();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // 首行可能是会话元数据(如 {"meta":{"headless":true,"schema":"…"}}),跳过
            if line.starts_with("{\"meta\"") {
                if meta_schema.is_none() {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                        meta_schema = v["meta"]["schema"].as_str().map(|s| s.to_string());
                    }
                }
                continue;
            }
            // 支持 {msg:{...}} 包装与裸消息两种格式
            let msg: ChatMessage = if line.starts_with("{\"msg\"") {
                let v: serde_json::Value = serde_json::from_str(line)?;
                serde_json::from_value(v["msg"].clone())?
            } else {
                serde_json::from_str(line)?
            };
            if msg.role == crate::llm::types::Role::System {
                continue;
            }
            self.messages.push(msg);
            loaded += 1;
        }
        // 旧版中断可能在历史里留下"声明了 tool_calls 却没有对应响应"的
        // 孤儿消息(见 repair_tool_chain),恢复时补齐,否则端点直接 400。
        let repaired = repair_tool_chain(&mut self.messages);
        // 把恢复的消息回传给 UI 展示(系统提示除外)
        let history_msgs: Vec<ChatMessage> = self
            .messages
            .iter()
            .filter(|m| m.role != crate::llm::types::Role::System)
            .cloned()
            .collect();
        if !history_msgs.is_empty() {
            self.emit(SessionEvent::HistoryLoaded {
                count: loaded,
                messages: history_msgs,
            });
        }
        if repaired > 0 {
            self.emit(SessionEvent::Notice(format!(
                "⚠ 历史里有 {repaired} 个中断遗留的未完成工具调用,已自动补齐占位。"
            )));
        }
        // 历史格式版本不符(他版/外部写入):提示已按当前格式尽力恢复
        if let Some(s) = &meta_schema {
            if s != HISTORY_SCHEMA {
                self.emit(SessionEvent::Notice(format!(
                    "⚠ 该历史会话由其他版本写入(格式标记 {s}),已按当前格式尽力恢复。"
                )));
            }
        }
        Ok(loaded)
    }

    fn push_system_prompt(&mut self) {
        // 避免重复注入
        if self
            .messages
            .first()
            .map(|m| m.role == crate::llm::types::Role::System)
            .unwrap_or(false)
        {
            return;
        }
        let sys = build_system_prompt(&self.cwd, self.mode, &self.session_id, &self.persona);
        self.messages.insert(0, ChatMessage::system(sys));
    }

    /// 首条为系统提示时,用当前状态重建(模式/人格/记忆摘要可能已变化)
    fn refresh_system_prompt(&mut self) {
        if self
            .messages
            .first()
            .map(|m| m.role == crate::llm::types::Role::System)
            .unwrap_or(false)
        {
            let sys = build_system_prompt(&self.cwd, self.mode, &self.session_id, &self.persona);
            self.messages[0] = ChatMessage::system(sys);
        }
    }

    /// 当前全局人格名(空 = 未注入)
    pub fn current_persona(&self) -> &str {
        &self.persona
    }

    /// 切换全局人格:空串/「无」= 关闭。写回 config(跨会话持久),
    /// 重建系统提示立即生效——影响之后所有对话。
    pub fn set_persona(&mut self, name: &str) -> anyhow::Result<String> {
        let name = name.trim();
        let effective = if name.is_empty() || name == "无" {
            String::new()
        } else if crate::persona::persona_text(name).is_some() {
            name.to_string()
        } else {
            anyhow::bail!("没有这个人格:「{name}」。输入 /persona 查看可用列表。");
        };
        self.persona = effective.clone();
        if let Ok(mut cfg) = crate::config::Config::load() {
            let _ = cfg.save_persona(&effective);
        }
        self.refresh_system_prompt();
        let label = if effective.is_empty() {
            "无(默认助手)".to_string()
        } else {
            format!("「{effective}」")
        };
        let msg = format!("✔ 人格已切换:{label}。全局生效并已写入配置,之后的对话都带着它;输入 /persona 无 可关闭。");
        self.emit(SessionEvent::Notice(msg.clone()));
        self.emit(SessionEvent::PersonaChanged { name: effective });
        Ok(msg)
    }

    /// 首次写入时创建历史文件并写 meta 首行(全会话一致;load 时跳过该行)。
    /// headless 标记供 resume 列表区分命令行 -p 会话;schema 是历史格式版本。
    fn ensure_history_writer(&mut self) {
        if self.history_writer.is_some() {
            return;
        }
        let Some(path) = self.history_path.clone() else {
            return;
        };
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(
                f,
                r#"{{"meta":{{"headless":{},"schema":"{}"}}}}"#,
                self.events.is_none(),
                HISTORY_SCHEMA
            );
            let _ = f.flush();
            self.history_writer = Some(f);
        }
    }

    /// 记录一条消息(内存 + 可选历史文件)
    fn record(&mut self, msg: &ChatMessage) {
        self.messages.push(msg.clone());
        self.ensure_history_writer();
        if let Some(f) = &mut self.history_writer {
            if let Ok(line) = serde_json::to_string(&msg) {
                let _ = writeln!(f, "{line}");
                let _ = f.flush();
            }
        }
    }

    fn emit(&self, ev: SessionEvent) {
        if let Some(tx) = &self.events {
            let _ = tx.send(ev);
        }
    }

    /// 向 UI 发送一条通知(供宿主进程调用)
    pub fn notify(&self, msg: impl Into<String>) {
        self.emit(SessionEvent::Notice(msg.into()));
    }

    /// 权限总判定:YOLO 直接放行 → 命令风险判定(高危/无法判定)→ 无头按档位静态判 /
    /// 交互弹窗问。
    ///
    /// 命令风险来自 permissions::inspect_command(启发式,解析目标路径而非匹配原文):
    /// - 解析出高危目标 → 交互模式弹红色确认框、只允许单次放行(不给"本会话都允许");
    ///   无头模式直接拒(bypassPermissions 也不放行)。
    /// - 目标无法判定(变量未知/通配符越界/间接调用)→ 同样要人点头,且不吃"本会话都允许"。
    async fn check_permission(&mut self, kind: PermKind, cmd: &str) -> PermissionResult {
        // YOLO(超级模式):一切放行、无任何问询,危险命令判定也跳过
        if self.mode == Mode::Yolo {
            return PermissionResult::Allowed;
        }
        // 高危(true)或无法判定(false)时携带给 UI 的说明
        let mut hazard: Option<(bool, String)> = None;
        if kind == PermKind::Command && !cmd.is_empty() {
            match inspect_command(cmd, &self.cwd) {
                CommandRisk::Blocked { reason, target } => {
                    if self.events.is_none() {
                        return PermissionResult::Denied(format!(
                            "命令被安全策略拦截: {reason}(解析出的目标:{target})。\
                             无头模式无法交互确认,已拒绝;如确需执行,请自行在终端跑"
                        ));
                    }
                    hazard = Some((true, format!("⚠ {reason}\n解析出的目标:{target}")));
                }
                CommandRisk::Uncertain { reason, detail } => {
                    if self.events.is_none() {
                        return match self.mode {
                            Mode::BypassPermissions => PermissionResult::Allowed,
                            _ => PermissionResult::Denied(format!(
                                "命令目标无法确定({reason}:{detail}),无头模式已拒绝。\
                                 要自动执行就加 --permission bypassPermissions"
                            )),
                        };
                    }
                    hazard = Some((
                        false,
                        format!("⚠ {reason}\n{detail}\n(含变量/通配符或间接调用,判定看不穿)"),
                    ));
                }
                CommandRisk::Safe => {}
            }
        }
        let has_warning = hazard.is_some();
        let warning = hazard.map(|(_, w)| w);

        if self.events.is_none() {
            match kind {
                PermKind::Write => match self.mode {
                    Mode::Ask => PermissionResult::Denied(
                        "ask 模式下写文件要确认,无头模式直接拒绝。\
                         要自动改文件就加 --permission acceptEdits"
                            .into(),
                    ),
                    _ => PermissionResult::Allowed,
                },
                PermKind::Command => match self.mode {
                    Mode::BypassPermissions => PermissionResult::Allowed,
                    _ => PermissionResult::Denied(
                        "该模式下跑命令要确认,无头模式直接拒绝。\
                         要自动执行就加 --permission bypassPermissions"
                            .into(),
                    ),
                },
            }
        } else {
            match kind {
                PermKind::Write if self.file_always => PermissionResult::Allowed,
                // 高危/无法判定的命令不吃"本会话都允许"
                PermKind::Command if self.command_always && !has_warning => {
                    PermissionResult::Allowed
                }
                _ => {
                    let (tx, rx) = oneshot::channel();
                    let title = match kind {
                        PermKind::Write => "文件写入确认",
                        PermKind::Command => "命令执行确认",
                    }
                    .to_string();
                    let body = match kind {
                        PermKind::Write => "znaide 想要修改文件".to_string(),
                        PermKind::Command => cmd.to_string(),
                    };
                    self.emit(SessionEvent::PermissionRequest { kind, title, body, warning, tx });
                    tokio::select! {
                        r = rx => match r {
                            Ok((allow, always)) => {
                                if allow && always && !has_warning {
                                    match kind {
                                        PermKind::Write => self.file_always = true,
                                        PermKind::Command => self.command_always = true,
                                    }
                                    self.emit(SessionEvent::Notice("🔓 本会话都自动放行这类操作了".into()));
                                }
                                if allow { PermissionResult::Allowed }
                                else { PermissionResult::Denied("用户拒绝了该操作".into()) }
                            }
                            Err(_) => PermissionResult::Denied("确认通道已关闭".into()),
                        },
                        _ = self.cancel.cancelled() => PermissionResult::Denied("用户中断了确认".into()),
                    }
                }
            }
        }
    }

    /// 跑一个完整回合:user prompt(@已展开)→ 工具循环 → 最终回复。
    /// 出错时,交互模式先发错误提示和 TurnFinished 复位忙态再返回 Err,
    /// 免得 UI 卡死在"正在思考与执行";无头模式直接 Err 交给调用方。
    pub async fn run_turn(&mut self, prompt: &str) -> anyhow::Result<TurnResult> {
        let result = self.run_turn_inner(prompt).await;
        if let Err(e) = &result {
            if self.events.is_some() {
                self.emit(SessionEvent::Notice(format!(
                    "⚠ 请求失败: {}",
                    describe_request_error(e)
                )));
                self.emit(SessionEvent::TurnFinished {
                    text: String::new(),
                    truncated: false,
                });
            }
        }
        result
    }

    /// 回合主体(错误由 run_turn 包装成 UI 事件)
    async fn run_turn_inner(&mut self, prompt: &str) -> anyhow::Result<TurnResult> {
        let expanded = expand_at_refs(prompt, &self.cwd);
        self.record(&ChatMessage::user(expanded));
        self.emit(SessionEvent::TurnStarted);

        let mut tool_calls = 0;
        // 本回合真实 token 累计(每次模型调用返回时累加,输入/输出分开计)
        let mut usage_in = 0u64;
        let mut usage_out = 0u64;
        // 工具定义 = 内置 + skill 入口 + MCP
        let mut defs = tools::ToolRegistry::builtin().defs();
        if let Some(skill_def) = build_skill_tool_def(&self.cwd) {
            defs.push(skill_def);
        }
        defs.extend(self.mcp.tool_defs());

        // D1 轮数预算:max_turns = 0 视为不限;每批用完问用户要不要续跑(D2)
        let batch = if self.max_turns == 0 { usize::MAX } else { self.max_turns };
        let mut budget = batch;
        let mut used = 0usize;
        // D3 刹车:完全相同(工具 + 参数)的调用计数
        let mut sig_seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

        loop {
            if used >= budget {
                if !self.ask_continue_rounds(used).await {
                    // 无人可问(无头)或用户选择就此收尾:说清怎么调大,别只说"没干完"
                    let text = if self.events.is_some() {
                        format!(
                            "⚠ 已达 {used} 轮上限,按你的选择停在这里。要继续直接再发一条消息;\
                             想一次跑更久可用 --max-turns N 调大上限(--max-turns 0 = 不限)"
                        )
                    } else {
                        format!(
                            "⚠ 已达 {used} 轮上限,任务可能没干完。用 --max-turns N 调大上限,\
                             --max-turns 0 表示不限"
                        )
                    };
                    self.emit(SessionEvent::TurnFinished { text: text.clone(), truncated: true });
                    return Ok(TurnResult {
                        text,
                        tool_calls,
                        truncated: true,
                        input_tokens: usage_in,
                        output_tokens: usage_out,
                    });
                }
                budget = budget.saturating_add(batch);
            }
            used += 1;
            // 状态栏/日志用:本轮第几轮、上限(0 = 不限)
            self.emit(SessionEvent::RoundStarted {
                used,
                limit: if budget == usize::MAX { 0 } else { budget },
            });
            let reply = if self.events.is_some() {
                let ev = self.events.clone();
                let cancel = self.cancel.clone();
                let mut content_buf = String::new();
                let mut reasoning_buf = String::new();
                let result = tokio::select! {
                    r = self.llm.chat_stream(&self.messages, Some(&defs), |evt| {
                        match evt {
                            StreamEvent::TextDelta(t) => {
                                content_buf.push_str(&t);
                                if let Some(tx) = &ev { let _ = tx.send(SessionEvent::TextDelta(t)); }
                            }
                            StreamEvent::ReasoningDelta(t) => {
                                reasoning_buf.push_str(&t);
                                if let Some(tx) = &ev { let _ = tx.send(SessionEvent::ReasoningDelta(t)); }
                            }
                        }
                    }) => r,
                    _ = cancel.cancelled() => {
                        let text = if content_buf.is_empty() { None } else { Some(content_buf.clone()) };
                        let r = AssistantReply { content: text, reasoning_content: None, tool_calls: vec![], usage: Default::default() };
                        self.record(&finalize_assistant(r, true));
                        let text = format!(
                            "⏹ 生成已中断{}",
                            if reasoning_buf.is_empty() { "" } else { "(已有部分思考,未输出正文)" }
                        );
                        self.emit(SessionEvent::TurnFinished { text: text.clone(), truncated: true });
                        return Ok(TurnResult { text, tool_calls, truncated: true, input_tokens: usage_in, output_tokens: usage_out });
                    }
                };
                result?
            } else {
                self.llm.chat(&self.messages, Some(&defs)).await?
            };

            let has_tools = !reply.tool_calls.is_empty();
            // 真实用量累计;每次调用结束发一个 Usage 事件给 UI(实时统计用)
            if !reply.usage.is_empty() {
                usage_in += reply.usage.prompt_tokens;
                usage_out += reply.usage.completion_tokens;
                self.emit(SessionEvent::Usage {
                    prompt: reply.usage.prompt_tokens,
                    completion: reply.usage.completion_tokens,
                });
            }
            self.record(&finalize_assistant(reply.clone(), false));

            if !has_tools {
                let text = reply
                    .content
                    .clone()
                    .or(reply.reasoning_content.clone())
                    .unwrap_or_else(|| "(模型无文本输出)".to_string());
                self.emit(SessionEvent::TurnFinished { text: text.clone(), truncated: false });
                return Ok(TurnResult { text, tool_calls, truncated: false, input_tokens: usage_in, output_tokens: usage_out });
            }

            // 逐个执行工具。Esc 中断后不再跑同批剩余的,但要给它们补齐
            // 占位响应再直接收尾——否则历史里会留下"assistant 声明了
            // tool_calls 却没有对应 tool 响应"的孤儿,恢复会话后端点 400。
            for (i, tc) in reply.tool_calls.iter().enumerate() {
                tool_calls += 1;
                let mut out = self.execute_tool_call(tc).await;
                // D3:同一 (工具, 参数) 重复调用先插一句提醒,给模型自救机会
                let sig = format!("{}|{}", tc.function.name, tc.function.arguments);
                let n = {
                    let c = sig_seen.entry(sig).or_insert(0);
                    *c += 1;
                    *c
                };
                if n >= REPEAT_WARN && n < REPEAT_ABORT {
                    out.push_str(&format!(
                        "\n\n⚠ 同一个调用(工具 + 参数完全一样)你已经重复第 {n} 次了,结果不会变。\
                         换参数/换工具/换思路,或直接汇报现状与卡点;再重复到 {REPEAT_ABORT} 次本轮会被中止。"
                    ));
                }
                self.record(&ChatMessage::tool(&tc.id, out));
                if self.cancel.is_cancelled() {
                    for rest in &reply.tool_calls[i + 1..] {
                        self.record(&ChatMessage::tool(&rest.id, PLACEHOLDER_TOOL_REPLY));
                    }
                    let text = "⏹ 生成已中断".to_string();
                    self.emit(SessionEvent::TurnFinished {
                        text: text.clone(),
                        truncated: true,
                    });
                    return Ok(TurnResult {
                        text,
                        tool_calls,
                        truncated: true,
                        input_tokens: usage_in,
                        output_tokens: usage_out,
                    });
                }
            }
            // D3:重复到上限还没换路 → 判定原地打转,收尾(比烧到轮数上限更早、说得更明白)
            if let Some((sig, n)) = sig_seen.iter().max_by_key(|(_, c)| **c) {
                if *n >= REPEAT_ABORT {
                    let name = sig.split('|').next().unwrap_or("?").to_string();
                    let text = format!(
                        "⚠ 检测到原地打转:工具 `{name}` 用完全相同的参数重复了 {n} 次,已中止本轮。\
                         换个思路再来,或直接告诉我要什么。"
                    );
                    self.emit(SessionEvent::TurnFinished { text: text.clone(), truncated: true });
                    return Ok(TurnResult {
                        text,
                        tool_calls,
                        truncated: true,
                        input_tokens: usage_in,
                        output_tokens: usage_out,
                    });
                }
            }
        }
    }

    /// D2:轮次预算用尽时问 UI 要不要再放一批。无头模式(没有 UI)直接 false。
    async fn ask_continue_rounds(&mut self, used: usize) -> bool {
        if self.events.is_none() {
            return false;
        }
        let (tx, rx) = oneshot::channel();
        self.emit(SessionEvent::RoundsExhausted { used, tx });
        tokio::select! {
            r = rx => r.unwrap_or(false),
            _ = self.cancel.cancelled() => false,
        }
    }

    /// 执行单个工具调用(权限 → 分发),返回回填给模型的结果文本
    async fn execute_tool_call(&mut self, tc: &ToolCall) -> String {
        let name = tc.function.name.clone();
        let args = match parse_tool_args(&tc.function.arguments) {
            Ok(v) => v,
            Err(detail) => {
                let msg = format!(
                    "工具参数解析失败:{detail}\n请按工具定义的 JSON Schema 重新构造 arguments 后再次调用。"
                );
                self.emit(SessionEvent::ToolStarted { name: name.clone(), args: String::new(), idle_ms: None });
                self.emit(SessionEvent::ToolFinished { name: name.clone(), ok: false, output: msg.clone() });
                return msg;
            }
        };

        // skill:模型自主调用。正文注入无权限面;若声明 entry,脚本按命令权限执行
        if name == "skill" {
            let skill_name = args
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let skill_args = args
                .get("args")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // 工具卡片直接显示技能名,而不是笼统的 "skill"
            let card = if skill_name.is_empty() {
                "skill".to_string()
            } else {
                format!("skill「{skill_name}」")
            };
            self.emit(SessionEvent::ToolStarted { name: card.clone(), args: pretty_args(&args), idle_ms: None });
            let (out, ok) = self.run_skill(&skill_name, &skill_args).await;
            let out = truncate_for_ui(&out);
            self.emit(SessionEvent::ToolFinished { name: card, ok, output: out.clone() });
            return out;
        }

        // MCP 工具(mcp__<server>__<tool>)
        if name.starts_with("mcp__") && self.mcp.has_tool(&name) {
            let perm = self.check_permission(PermKind::Write, "").await;
            return match perm {
                PermissionResult::Denied(reason) => {
                    self.emit(SessionEvent::ToolStarted { name: name.clone(), args: pretty_args(&args), idle_ms: None });
                    self.emit(SessionEvent::ToolFinished { name: name.clone(), ok: false, output: reason.clone() });
                    format!("操作未执行: {reason}")
                }
                PermissionResult::Allowed => {
                    self.emit(SessionEvent::ToolStarted { name: name.clone(), args: pretty_args(&args), idle_ms: None });
                    match self.mcp.call(&name, args).await {
                        Ok(out) => {
                            let out = truncate_for_ui(&out);
                            self.emit(SessionEvent::ToolFinished { name: name.clone(), ok: true, output: out.clone() });
                            out
                        }
                        Err(e) => {
                            let msg = format!("MCP 工具 {name} 执行失败: {e}");
                            self.emit(SessionEvent::ToolFinished { name: name.clone(), ok: false, output: msg.clone() });
                            msg
                        }
                    }
                }
            };
        }

        let need_perm = match name.as_str() {
            "write_file" | "edit" => Some(PermKind::Write),
            "run_shell_command" => {
                let cmd = args
                    .get("command")
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .to_string();
                if cmd.trim().is_empty() {
                    let msg = "command 不能为空:请传入要执行的完整命令字符串,如 {\"command\": \"ls -la\"}".to_string();
                    self.emit(SessionEvent::ToolStarted { name: name.clone(), args: String::new(), idle_ms: tool_idle_ms(&name, &args) });
                    self.emit(SessionEvent::ToolFinished { name: name.clone(), ok: false, output: msg.clone() });
                    return msg;
                }
                let perm = self.check_permission(PermKind::Command, &cmd).await;
                return self.after_permission(name, args, perm).await;
            }
            _ => None,
        };

        if let Some(kind) = need_perm {
            let perm = self.check_permission(kind, "").await;
            self.after_permission(name, args, perm).await
        } else {
            self.after_permission(name, args, PermissionResult::Allowed).await
        }
    }

    /// 权限通过后执行工具
    async fn after_permission(
        &self,
        name: String,
        args: Value,
        perm: PermissionResult,
    ) -> String {
        match perm {
            PermissionResult::Denied(reason) => {
                self.emit(SessionEvent::ToolStarted { name: name.clone(), args: pretty_args(&args), idle_ms: tool_idle_ms(&name, &args) });
                self.emit(SessionEvent::ToolFinished { name: name.clone(), ok: false, output: reason.clone() });
                format!("操作未执行: {reason}")
            }
            PermissionResult::Allowed => {
                self.emit(SessionEvent::ToolStarted { name: name.clone(), args: pretty_args(&args), idle_ms: tool_idle_ms(&name, &args) });
                // 会话已放行:工具内权限给"全放行";YOLO 会话透传 YOLO,连高危命令判定也跳过
                let tool_mode = if self.mode == Mode::Yolo {
                    Mode::Yolo
                } else {
                    Mode::BypassPermissions
                };
                let perm = Permission::new(tool_mode);
                let ctx = ToolContext {
                    cwd: &self.cwd,
                    permission: &perm,
                    session_id: &self.session_id,
                    cancel: Some(self.cancel.clone()),
                    events: self.events.as_ref(),
                };
                match tools::execute(&name, args, &ctx).await {
                    Ok(out) => {
                        self.emit(SessionEvent::ToolFinished { name: name.clone(), ok: true, output: out.clone() });
                        out
                    }
                    Err(e) => {
                        let msg = format!("工具 {name} 执行失败: {e}");
                        self.emit(SessionEvent::ToolFinished { name: name.clone(), ok: false, output: msg.clone() });
                        msg
                    }
                }
            }
        }
    }

    /// 技能唯一运行器(模型调用与手动触发共用):渲染正文,
    /// 有 entry 脚本就按命令权限执行,把输出并进去。返回 (文本, 是否成功)。
    async fn run_skill(&mut self, name: &str, args: &str) -> (String, bool) {
        let set = crate::skills::scan(&self.cwd);
        let Some(skill) = crate::skills::find(&set.skills, name) else {
            let avail: Vec<&str> = set.skills.iter().map(|s| s.name.as_str()).collect();
            let msg = if avail.is_empty() {
                "未找到该技能(当前没有任何已安装的技能,可参考 README 的 skill 示例)。".to_string()
            } else {
                format!(
                    "未找到技能「{name}」。可用技能:{}(输入 /skills 查看详情)", avail.join(", ")
                )
            };
            return (msg, false);
        };

        let mut out = skill.render(args);
        if skill.entry.is_some() {
            // 入口脚本 = 命令执行:先确认当前平台有可用脚本,再走权限三档判定
            match crate::skills::entry_command(skill, args) {
                Err(reason) => {
                    out.push_str(&format!(
                        "\n\n[入口脚本未执行:{reason}]\n(本次仅按正文说明执行)\n"
                    ));
                }
                Ok(cmd) => {
                    let perm = self.check_permission(PermKind::Command, &cmd).await;
                    match perm {
                        PermissionResult::Denied(reason) => {
                            out.push_str(&format!("\n\n[入口脚本未执行:{reason}]\n"));
                        }
                        PermissionResult::Allowed => {
                            // 实际执行复用 run_shell_command 工具通道(bash/cmd、超时、输出截断)。
                            // 入口脚本是用户自己写的,按默认静默预算(90s)走,产出续命
                            let run_args = json!({
                                "command": cmd,
                            });
                            // 同 after_permission:YOLO 会话透传 YOLO(入口脚本也跳过高危判定)
                            let tool_mode = if self.mode == Mode::Yolo {
                                Mode::Yolo
                            } else {
                                Mode::BypassPermissions
                            };
                            let perm = Permission::new(tool_mode);
                            let ctx = ToolContext {
                                cwd: &self.cwd,
                                permission: &perm,
                                session_id: &self.session_id,
                                cancel: Some(self.cancel.clone()),
                                events: self.events.as_ref(),
                            };
                            match tools::execute("run_shell_command", run_args, &ctx).await {
                                Ok(o) => {
                                    out.push_str(&format!("\n\n[入口脚本输出]\n{o}"));
                                }
                                Err(e) => {
                                    out.push_str(&format!("\n\n[入口脚本执行失败:{e}]\n"));
                                }
                            }
                        }
                    }
                }
            }
        }
        (out, true)
    }

    /// 手动触发技能:/名字 [参数]。渲染正文(+ 入口输出)后当一个新回合跑;
    /// 技能不存在就只提示、不开回合(免得 UI 的 busy 状态没人清)。
    pub async fn run_skill_turn(
        &mut self,
        name: &str,
        args: &str,
    ) -> anyhow::Result<TurnResult> {
        let (content, ok) = self.run_skill(name, args).await;
        if !ok {
            self.notify(content);
            self.emit(SessionEvent::TurnFinished { text: String::new(), truncated: false });
            return Ok(TurnResult { text: String::new(), tool_calls: 0, truncated: false, input_tokens: 0, output_tokens: 0 });
        }
        self.run_turn(&content).await
    }
}

enum PermissionResult {
    Allowed,
    Denied(String),
}

/// 把请求错误翻成一句给人看的提示:网络层错误给可操作的中文,
/// 其余错误(端点业务错误、参数解析等)回原始错误链(err:# 含全部原因)。
pub fn describe_request_error(err: &anyhow::Error) -> String {
    let re = err.chain().find_map(|c| c.downcast_ref::<reqwest::Error>());
    match re {
        Some(re) if re.is_connect() && re.is_timeout() => {
            "连模型端点超时了(10s 内没建立连接):多半是网络不通或代理问题,查下 base_url 通不通".into()
        }
        Some(re) if re.is_timeout() => {
            "请求模型端点超时(模型半天没响应):稍后再试,或看看模型服务负载".into()
        }
        Some(re) if re.is_connect() => {
            "无法连接模型端点(连接/DNS 失败):检查 base_url、网络和代理设置".into()
        }
        Some(re) if re.is_body() => {
            "读模型响应读到一半断了(连接被关或网络抖动):可重试一次".into()
        }
        Some(re) => format!("模型请求失败: {re}"),
        None => format!("{err:#}"),
    }
}

/// 组 `skill` 工具定义:单一入口 + enum 可用列表,上下文不随技能数膨胀、
/// 命名无冲突;disable-model-invocation 的不进列表。每回合动态生成。
fn build_skill_tool_def(cwd: &Path) -> Option<ToolDef> {
    let set = crate::skills::scan(cwd);
    let visible = crate::skills::model_visible(&set.skills);
    if visible.is_empty() {
        return None;
    }
    let names: Vec<String> = visible.iter().map(|s| s.name.clone()).collect();
    let mut desc = String::from(
        "启用一个技能(skill):技能 = 针对某类任务的成套操作说明。\
         调用后你会收到详细步骤,再按步骤调用其它工具执行。可选技能:\n",
    );
    for s in visible.iter().take(30) {
        desc.push_str(&format!("- {}: {}\n", s.name, s.description));
    }
    if visible.len() > 30 {
        desc.push_str(&format!("…(共 {} 个)", visible.len()));
    }
    Some(ToolDef::function(
        "skill",
        desc,
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "enum": names, "description": "要启用的技能名"},
                "args": {"type": "string", "description": "传给技能的自由文本参数(可省略)"}
            },
            "required": ["name"]
        }),
    ))
}

/// run_shell_command 的工具卡片要显示静默预算:解析模型传值(兼容旧名
/// timeout_ms)并夹取生效值;其它工具没有预算概念,返回 None
fn tool_idle_ms(name: &str, args: &Value) -> Option<u64> {
    if name != "run_shell_command" {
        return None;
    }
    let raw = args
        .get("idle_ms")
        .and_then(Value::as_u64)
        .or_else(|| args.get("timeout_ms").and_then(Value::as_u64));
    // 没传(模型没承诺时长)返回 None,UI 不显示预算段
    raw.map(|v| v.min(crate::tools::shell::MAX_BUDGET_MS))
}

fn pretty_args(args: &Value) -> String {
    match args {
        Value::Null => String::new(),
        _ => serde_json::to_string(args).unwrap_or_default(),
    }
}

/// 解析模型传来的工具参数并归一化:空串/纯空白或非对象 → {};截断型笔误 → 修补;
/// 其它非法 JSON → 报错并附原文。初衷:弱模型省略参数时别再报 "invalid type: null"。
fn parse_tool_args(raw: &str) -> Result<Value, String> {
    let t = raw.trim();
    if t.is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    match serde_json::from_str::<Value>(t) {
        Ok(v @ Value::Object(_)) => return Ok(v),
        Ok(_) => return Ok(Value::Object(Default::default())), // null/标量/数组 → {}
        Err(_) => {}
    }
    // 截断修补:模型常漏掉字符串结尾的引号或对象结尾的花括号
    if let Some(v) = repair_truncated_json(t) {
        return Ok(v);
    }
    let shown: String = raw.chars().take(200).collect();
    Err(format!("不是合法的 JSON 对象: 「{shown}」"))
}

/// 修补截断型 JSON 笔误:每个候选都重新解析,能完整解析成对象才收,不猜字段。
fn repair_truncated_json(raw: &str) -> Option<Value> {
    let body = raw.trim_end();
    if body.is_empty() {
        return None;
    }
    let mut cands: Vec<String> = Vec::new();
    if body.ends_with('}') {
        // 右花括号在,但最后一个字符串没闭合,如 {"command":"ls -la} → 在 } 前补 "
        //(必须先试这个,否则补在末尾会把 } 吞进字符串值里)
        let (head, tail) = body.split_at(body.len() - 1);
        cands.push(format!("{head}\"{tail}"));
    }
    // 结尾引号与右花括号都丢了,如 {"command":"ls -la  → 补 "}
    cands.push(format!("{body}\"}}"));
    // 对象少了最外层右花括号(字符串已闭合),如 {"command":"ls -la"  → 补 }
    cands.push(format!("{body}}}"));
    for c in cands {
        if let Ok(v @ Value::Object(_)) = serde_json::from_str(&c) {
            return Some(v);
        }
    }
    None
}

/// 工具调用在中断时未执行的统一占位响应:给模型看的补全文本,
/// 保证每个 tool_call_id 都有对应 tool 消息(端点硬性要求)
const PLACEHOLDER_TOOL_REPLY: &str = "⏹ 用户中断,该工具调用未执行。";

/// 历史文件格式版本:每个会话文件首行 meta 的 schema 字段。
/// 解析跳过 meta 行;将来格式变化时按版本决定兼容/迁移。
const HISTORY_SCHEMA: &str = "c4a23d6e50c7";

/// 清洗"中断遗留的孤儿 tool_calls":assistant 声明了 N 个工具调用,
/// 却因 Esc 中断只执行了一部分(甚至一个都没跑),缺失的响应在这里补
/// 占位;错插在 tool 链中间的 assistant 文本会被顺延到补齐后的链尾。
/// 返回补了几条。恢复老会话后,内存里的消息序列就合法了。
fn repair_tool_chain(messages: &mut Vec<ChatMessage>) -> usize {
    let mut pending: Vec<String> = Vec::new();
    let mut out: Vec<ChatMessage> = Vec::with_capacity(messages.len());
    let mut fixed = 0usize;
    for m in messages.drain(..) {
        // 非 tool 消息是 tool 链的终止符:放行前先把欠的响应补齐
        if m.role != crate::llm::types::Role::Tool && !pending.is_empty() {
            for id in pending.drain(..) {
                out.push(ChatMessage::tool(id, PLACEHOLDER_TOOL_REPLY));
                fixed += 1;
            }
        }
        match m.role {
            crate::llm::types::Role::Assistant => {
                if let Some(calls) = &m.tool_calls {
                    if !calls.is_empty() {
                        for c in calls {
                            if !pending.contains(&c.id) {
                                pending.push(c.id.clone());
                            }
                        }
                    }
                }
                out.push(m);
            }
            crate::llm::types::Role::Tool => {
                let matched = m
                    .tool_call_id
                    .as_ref()
                    .map(|id| {
                        if let Some(pos) = pending.iter().position(|p| p == id) {
                            pending.remove(pos);
                            true
                        } else {
                            false
                        }
                    })
                    .unwrap_or(false);
                // 找不到对应声明的 tool 消息(声明方已不在):丢掉,
                // 留着也会被端点拒(无主 tool 消息同样非法)
                if matched {
                    out.push(m);
                }
            }
            _ => out.push(m),
        }
    }
    // 历史结尾还欠着响应(最后一条是带 tool_calls 的 assistant)
    for id in pending.drain(..) {
        out.push(ChatMessage::tool(id, PLACEHOLDER_TOOL_REPLY));
        fixed += 1;
    }
    *messages = out;
    fixed
}

/// 工具输出要回给模型、也要显示,太长就截断,免得撑爆上下文/刷屏
fn truncate_for_ui(s: &str) -> String {
    const MAX: usize = 20_000;
    if s.chars().count() <= MAX {
        s.to_string()
    } else {
        let head: String = s.chars().take(MAX).collect();
        format!("{head}\n…(结果过长已截断)")
    }
}

/// 组装 assistant 消息
fn finalize_assistant(mut reply: AssistantReply, interrupted: bool) -> ChatMessage {
    if interrupted {
        if let Some(c) = &mut reply.content {
            c.push_str("\n[⏹ 生成被用户中断]");
        }
    }
    let content = reply.content.clone().filter(|c| !c.is_empty());
    if reply.tool_calls.is_empty() {
        ChatMessage::assistant(content.unwrap_or_default())
    } else {
        ChatMessage::assistant_with_tool_calls(content, reply.tool_calls.clone())
    }
}

/// 组装系统提示词(身份/人设/环境/工具须知/记忆摘要)
fn build_system_prompt(cwd: &Path, mode: Mode, session_id: &str, persona: &str) -> String {
    let mode_name = match mode {
        Mode::Ask => "ask(文件写入、命令执行均需用户确认)",
        Mode::AcceptEdits => "acceptEdits(文件修改自动放行,命令执行需确认)",
        Mode::BypassPermissions => "bypassPermissions(全自动放行,危险命令除外)",
        Mode::Yolo => "YOLO(超级模式:一切放行、无确认、含危险命令;请务必谨慎判断每一步的后果)",
    };
    let memory_hint = memory_hint();
    // 全局人格:改写"身份段"。人设只管表达与视角,工作守则/安全边界是
    // 其下的底层约束,任何时候不可被人设覆盖
    let persona_block = if persona.is_empty() {
        String::new()
    } else {
        match crate::persona::persona_text(persona) {
            Some(text) => format!(
                "\n\
## 人设\n\
你当前以「{persona}」的身份与用户相处,人设要求:\n\
{text}\n\
\n\
人设只管你怎么说话、怎么看待任务;下面的工作守则与安全边界是底层约束,任何时候都不可违背\
(危险命令照拦、工具照规范调用、权限确认照常,别把人设带进工具参数或危险操作里)。\n\
"
            ),
            None => String::new(),
        }
    };
    format!(
        "你是 znaide,运行在用户终端里的通用 AI 助手。用户会给你自然语言指令,你通过调用工具一步步完成。\n\
{persona_block}\
\n\
## 工作守则\n\
1. 先侦查再动手:不确定环境/文件内容时,先用 list_directory / read_file / glob / grep_search 了解情况,不要凭空猜测。\n\
2. 每次调用一个工具,观察结果后再决定下一步;需要多步时耐心推进,不要在一步里试图做完所有事。\n\
3. 工具报错后:阅读错误信息,修正参数重试;同一错误不要盲目重试超过 2 次。\n\
4. 调用工具时,arguments 必须是完整合法的 JSON 对象:键名与所有字符串值都要用双引号,冒号、逗号一个都不能少;命令类参数保持短小清晰,不要在里面拼超长/多层引号的命令。\n\
5. 修改文件用 write_file(新建/覆写)或 edit(精确替换);系统会自动 undo 备份,无需你操心。\n\
6. 执行命令用 run_shell_command;若被安全策略拦截或权限拒绝,如实告诉用户原因,不要尝试绕过。\n\
7. 调用 run_shell_command 时,若命令可能耗时较长(构建、下载、git clone、批量扫描等),把 idle_ms 传一个宽松的预估完成时长(如 300000~600000),宁大勿小——跑超预估会被终止并把运行进展反馈给你,超时被杀整轮白跑、重试更慢。\n\
8. 用户输入中的 @文件 或 @目录 已被自动展开为内容,可直接使用;若需查看更多,用工具读取。\n\
9. 最终回复用中文,简洁说明:你做了什么、结果如何、需要用户注意什么。\n\
\n\
## 当前环境\n\
- 工作目录(cwd): {cwd}\n\
- 权限模式: {mode_name}\n\
- 会话 id: {session_id}\n\
\n\
## 长期记忆\n\
下面是从你的记忆库读取的摘要(MEMORY.md 索引)。如需查看完整记忆内容或写入新记忆,使用 memory_read / memory_write 工具:\n\
{memory_hint}\n\
\n\
## 边界\n\
- 只能操作本机;远程服务器请通过 ssh/scp/rsync 等命令访问(命令权限受模式约束)。\n\
- 无法访问互联网时如实说明。\n",
        cwd = cwd.display(),
        mode_name = mode_name,
        session_id = session_id,
        memory_hint = memory_hint,
    )
}

/// 读 ~/.znaide/memories/MEMORY.md 摘要(存在时)
fn memory_hint() -> String {
    let p = crate::config::data_dir().join("memories").join("MEMORY.md");
    match std::fs::read_to_string(&p) {
        Ok(text) => {
            let lines: Vec<&str> = text.lines().collect();
            let shown: Vec<&str> = lines.iter().take(60).copied().collect();
            let mut out = String::new();
            for l in shown {
                out.push_str(l);
                out.push('\n');
            }
            if lines.len() > 60 {
                out.push_str(&format!("…(共 {} 行,如需全部内容用 memory_read)", lines.len()));
            }
            out
        }
        Err(_) => "(暂无记忆。若用户提到重要的环境信息/偏好,用 memory_write 记录)".into(),
    }
}

/// 一条历史会话(带无头标识与用户备注)
pub struct HistoryEntry {
    pub path: PathBuf,
    /// 命令行无头模式(-p)产生的会话(文件首行 meta 标记)
    pub headless: bool,
    /// 用户备注(会话旁 <id>.meta.json 的 note;无备注为 None)
    pub note: Option<String>,
}

/// 解析历史文件首行的会话元数据(仅 headless 标志;旧文件无 meta 视为交互会话)
fn entry_headless(path: &Path) -> bool {
    use std::io::BufRead;
    let Ok(f) = std::fs::File::open(path) else {
        return false;
    };
    let mut first = String::new();
    if std::io::BufReader::new(f)
        .read_line(&mut first)
        .ok()
        .map(|n| n == 0)
        .unwrap_or(true)
    {
        return false;
    }
    let first = first.trim();
    if !first.starts_with("{\"meta\"") {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(first)
        .ok()
        .and_then(|v| v.get("meta").and_then(|m| m.get("headless")).and_then(|h| h.as_bool()))
        .unwrap_or(false)
}

/// 会话备注 sidecar 路径:<id>.jsonl → 同目录 <id>.meta.json。
/// sidecar 是独立小文件(原子写),jsonl 保持纯消息流;jsonl 首行 meta 只放
/// headless/schema 这类随文件一生不变的内容(改备注不重写大文件,/clear
/// 截断 jsonl 也不丢备注)。
fn note_sidecar_path(jsonl: &Path) -> PathBuf {
    let stem = jsonl.file_stem().unwrap_or_default();
    jsonl.with_file_name(format!("{}.meta.json", stem.to_string_lossy()))
}

/// 读会话备注:无 sidecar / 坏 JSON / 空文本一律 None(不报错)。
pub fn read_note(jsonl: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(note_sidecar_path(jsonl)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let note = v.get("note")?.as_str()?.trim();
    if note.is_empty() {
        None
    } else {
        Some(note.to_string())
    }
}

/// 写会话备注(空文本 = 删除 sidecar)。原子:同目录临时文件 + rename,
/// 避免写一半崩溃留下坏 JSON。
pub fn write_note(jsonl: &Path, note: &str) -> std::io::Result<()> {
    let path = note_sidecar_path(jsonl);
    let note = note.trim();
    if note.is_empty() {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        };
    }
    let body = serde_json::json!({ "note": note }).to_string();
    let tmp = path.with_extension("tmp"); // <id>.meta.tmp,不会被会话列表扫到
    std::fs::write(&tmp, body)?;
    match std::fs::rename(&tmp, &path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// 删除会话文件及其备注 sidecar(/resume del、空壳清理共用;幂等)。
pub fn remove_session(jsonl: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(jsonl) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    match std::fs::remove_file(note_sidecar_path(jsonl)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    Ok(())
}

/// 列出 ~/.znaide/sessions 下所有历史会话(带无头标识,供 /resume)。
/// 空文件(0 字节,旧版本遗留的空壳)没有恢复价值,直接跳过。
pub fn list_history_sessions_detailed() -> Vec<HistoryEntry> {
    let dir = data_dir().join("sessions");
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().map(|x| x == "jsonl").unwrap_or(false)
                && e.metadata().map(|m| m.len() > 0).unwrap_or(false)
            {
                let note = read_note(&p);
                out.push(HistoryEntry {
                    headless: entry_headless(&p),
                    path: p,
                    note,
                });
            }
        }
    }
    out.sort_by_key(|h| {
        std::fs::metadata(&h.path)
            .and_then(|m| m.modified())
            .map(|t| {
                t.duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            })
            .unwrap_or(0)
    });
    out.reverse(); // 最新在前
    out
}

/// 列出 ~/.znaide/sessions 下所有历史会话(供 /resume)。
/// 空文件(0 字节,通常是刚启动还没对话的当前会话)没有恢复价值,直接跳过。
pub fn list_history_sessions() -> Vec<PathBuf> {
    list_history_sessions_detailed()
        .into_iter()
        .map(|h| h.path)
        .collect()
}

/// 清掉历史遗留的 0 字节空壳会话文件(懒创建后不再产生,启动时顺手清理旧账)。
/// 返回删了几个。
pub fn prune_empty_sessions() -> usize {
    let dir = data_dir().join("sessions");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let mut removed = 0usize;
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().map(|x| x == "jsonl").unwrap_or(false)
            && e.metadata().map(|m| m.len() == 0).unwrap_or(false)
            && remove_session(&p).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::parse_tool_args;
    use serde_json::json;

    #[test]
    fn empty_args_become_empty_object() {
        // 弱模型常整段省略参数:空串/纯空白都不能再报 "invalid type: null"
        for raw in ["", "   ", "\n"] {
            let v = parse_tool_args(raw).unwrap();
            assert!(v.is_object(), "raw={raw:?}");
            assert!(v.as_object().unwrap().is_empty());
        }
    }

    #[test]
    fn null_and_non_object_become_empty_object() {
        for raw in ["null", "\"hi\"", "123", "[1,2]"] {
            let v = parse_tool_args(raw).unwrap();
            assert!(v.is_object(), "raw={raw:?}");
            assert!(v.as_object().unwrap().is_empty());
        }
    }

    #[test]
    fn object_preserved() {
        let v = parse_tool_args(r#"{"command":"ls -la"}"#).unwrap();
        assert_eq!(v["command"], "ls -la");
        assert_eq!(
            parse_tool_args(r#"{"path":"src","offset":3}"#).unwrap(),
            json!({"path":"src","offset":3})
        );
    }

    #[test]
    fn invalid_json_returns_error_with_raw() {
        let e = parse_tool_args("{command: ls}").unwrap_err();
        assert!(e.contains("不是合法的 JSON 对象"));
        assert!(e.contains("command"));
        // 空串不算错误(归一化为 {})
        assert!(parse_tool_args("").is_ok());
    }

    #[test]
    fn truncated_json_is_repaired() {
        // 模型漏掉字符串结尾引号 + 对象右花括号 → 补 "}
        let v = parse_tool_args(r#"{"command":"du -sh /home/zngeek/.cache | sort -rh | head -n 10"#)
            .unwrap();
        assert_eq!(
            v["command"],
            "du -sh /home/zngeek/.cache | sort -rh | head -n 10"
        );
        // 右花括号在但字符串没闭合 → 在 } 前补引号(不能把 } 吞进命令值)
        let v = parse_tool_args(r#"{"command":"ls -la}"#).unwrap();
        assert_eq!(v["command"], "ls -la");
        // 字符串已闭合只缺右花括号 → 补 }
        let v = parse_tool_args(r#"{"command":"pwd","cwd":"/tmp""#).unwrap();
        assert_eq!(v["command"], "pwd");
        assert_eq!(v["cwd"], "/tmp");
        // 修补后仍然解析不出 → 报错而不是瞎猜
        assert!(parse_tool_args(r#"{"command": "ls" garbage"#).is_err());
    }

    /// 人格注入:system prompt 出现"人设"段,且声明守则为不可覆盖的底层约束;
    /// 无人格/未知人格不注入
    #[test]
    fn system_prompt_injects_persona_block() {
        let cwd = std::path::Path::new("/tmp");
        let plain = super::build_system_prompt(cwd, crate::permissions::Mode::Ask, "s1", "");
        assert!(!plain.contains("## 人设"));
        let yes = super::build_system_prompt(cwd, crate::permissions::Mode::Ask, "s1", "毒舌损友");
        assert!(yes.contains("## 人设"), "应注入人设段");
        assert!(yes.contains("「毒舌损友」"));
        assert!(yes.contains("底层约束"), "应声明工作守则不可被人设覆盖");
        let bad = super::build_system_prompt(cwd, crate::permissions::Mode::Ask, "s1", "不存在的角色");
        assert!(!bad.contains("## 人设"), "未知人格应静默不注入");
    }

    /// headless 元数据解析:首行 {"meta":{"headless":true}} → true;普通会话 → false
    #[test]
    fn entry_headless_parses_meta_line() {
        let dir = std::env::temp_dir().join(format!("znaide_hdr_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let h = dir.join("h.jsonl");
        let i = dir.join("i.jsonl");
        std::fs::write(&h, "{\"meta\":{\"headless\":true}}\n{\"role\":\"user\"...}\n").unwrap();
        std::fs::write(&i, "{\"role\":\"user\",\"content\":\"hi\"}\n").unwrap();
        assert!(super::entry_headless(&h), "headless 标记应识别");
        assert!(!super::entry_headless(&i), "无 meta 的普通会话应为 false");
        assert!(!super::entry_headless(&dir.join("not_exist.jsonl")), "文件缺失不应 panic");
        std::fs::remove_file(&h).ok();
        std::fs::remove_file(&i).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---------- 会话备注(sidecar <id>.meta.json) ----------

    /// 写读 roundtrip、覆盖更新、空文本清除(删除 sidecar)
    #[test]
    fn note_write_read_overwrite_clear() {
        let dir = std::env::temp_dir().join(format!("znaide_note_ut_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sess = dir.join("s1.jsonl");
        std::fs::write(&sess, "{\"meta\":{\"headless\":false}}\n").unwrap();

        // 初始无备注
        assert_eq!(super::read_note(&sess), None, "无 sidecar → None");
        // 写入 → 读回;首尾空白应裁剪
        super::write_note(&sess, "  给 README 做英文版  ").unwrap();
        assert_eq!(super::read_note(&sess).as_deref(), Some("给 README 做英文版"));
        // sidecar 落在 <id>.meta.json,不改动 jsonl 本体
        assert!(dir.join("s1.meta.json").exists());
        assert!(!dir.join("s1.meta.json.tmp").exists(), "临时文件应已改名");
        assert_eq!(
            std::fs::read_to_string(&sess).unwrap(),
            "{\"meta\":{\"headless\":false}}\n",
            "jsonl 内容不得被备注写入触碰"
        );
        // 覆盖更新
        super::write_note(&sess, "改成英文 README 的会话").unwrap();
        assert_eq!(super::read_note(&sess).as_deref(), Some("改成英文 README 的会话"));
        // 空文本 = 清除
        super::write_note(&sess, "   ").unwrap();
        assert_eq!(super::read_note(&sess), None, "空备注应清除");
        assert!(!dir.join("s1.meta.json").exists(), "sidecar 应被删除");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 坏 JSON / 缺字段 → None,不 panic
    #[test]
    fn note_bad_sidecar_gives_none() {
        let dir = std::env::temp_dir().join(format!("znaide_note_bad_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sess = dir.join("s2.jsonl");
        std::fs::write(&sess, "x").unwrap();
        // 坏 JSON
        std::fs::write(dir.join("s2.meta.json"), "{not json").unwrap();
        assert_eq!(super::read_note(&sess), None, "坏 JSON → None");
        // JSON 但无 note 字段
        std::fs::write(dir.join("s2.meta.json"), "{\"other\":1}").unwrap();
        assert_eq!(super::read_note(&sess), None);
        // note 是空串
        std::fs::write(dir.join("s2.meta.json"), "{\"note\":\"\"}").unwrap();
        assert_eq!(super::read_note(&sess), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 删除会话(jsonl)时 sidecar 一并删;缺失文件幂等
    #[test]
    fn remove_session_deletes_jsonl_and_sidecar() {
        let dir = std::env::temp_dir().join(format!("znaide_note_del_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sess = dir.join("s3.jsonl");
        std::fs::write(&sess, "x").unwrap();
        super::write_note(&sess, "要删的备注").unwrap();
        assert!(dir.join("s3.meta.json").exists());
        super::remove_session(&sess).unwrap();
        assert!(!sess.exists(), "jsonl 应被删");
        assert!(!dir.join("s3.meta.json").exists(), "sidecar 应一并删");
        // 幂等:再删不报错
        super::remove_session(&sess).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 空壳清理(0 字节 jsonl)应把孤儿 sidecar 一起清掉
    #[test]
    fn prune_empty_sessions_removes_sidecar_too() {
        // 依赖 ZNAIDE_DATA_DIR:与 undo 测试共用同一把锁串行(先锁后设 env)
        let _g = crate::test_util::DATA_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("znaide_sess_prune_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sessions")).unwrap();
        std::env::set_var("ZNAIDE_DATA_DIR", &dir);
        let sess = dir.join("sessions").join("ghost.jsonl");
        std::fs::write(&sess, "").unwrap();
        super::write_note(&sess, "孤儿备注").unwrap();
        assert_eq!(super::prune_empty_sessions(), 1, "应清掉 1 个空壳");
        assert!(!sess.exists());
        assert!(
            !dir.join("sessions").join("ghost.meta.json").exists(),
            "孤儿 sidecar 应随空壳一起删"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---------- repair_tool_chain:清洗中断遗留的孤儿 tool_calls ----------

    use crate::llm::types::{ChatMessage, FunctionCall, Role, ToolCall};

    fn tc(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "run_shell_command".into(),
                arguments: "{}".into(),
            },
        }
    }

    fn asst_with_calls(ids: &[&str]) -> ChatMessage {
        ChatMessage::assistant_with_tool_calls(
            Some("调一下工具".into()),
            ids.iter().map(|i| tc(i)).collect(),
        )
    }

    fn assistant(text: &str) -> ChatMessage {
        ChatMessage::assistant(text)
    }

    /// 按出现顺序收集 (role, tool_call_id 或内容片段),便于断言序列合法性
    fn fingerprint(msgs: &[ChatMessage]) -> Vec<(String, String)> {
        msgs.iter()
            .map(|m| {
                let id = m
                    .tool_call_id
                    .clone()
                    .or_else(|| {
                        m.tool_calls.as_ref().map(|cs| {
                            cs.iter().map(|c| c.id.clone()).collect::<Vec<_>>().join("+")
                        })
                    })
                    .unwrap_or_else(|| m.content.clone().unwrap_or_default());
                (m.role.as_str().to_string(), id)
            })
            .collect()
    }

    #[test]
    fn repair_fills_missing_tool_replies_before_stray_assistant() {
        // 复现线上事故:assistant 声明 2 个调用,只执行了 1 个,
        // 中断文本还插在 tool 链中间 → 端点 400 的元凶序列
        let mut msgs = vec![
            asst_with_calls(&["a", "b"]),
            ChatMessage::tool("a", "命令已中断"),
            assistant("⏹ 生成已中断"),
        ];
        let fixed = super::repair_tool_chain(&mut msgs);
        assert_eq!(fixed, 1, "应补 1 条占位(b 的响应)");
        let fp = fingerprint(&msgs);
        assert_eq!(
            fp,
            vec![
                ("assistant".into(), "a+b".into()),
                ("tool".into(), "a".into()),
                ("tool".into(), "b".into()),
                ("assistant".into(), "⏹ 生成已中断".into()),
            ],
            "占位要插在错插的 assistant 文本之前,tool 链必须完整连续"
        );
        assert!(msgs[2].content.as_deref() == Some(super::PLACEHOLDER_TOOL_REPLY));
    }

    #[test]
    fn repair_fills_when_no_tool_ran_at_all() {
        // 批里第一个工具还没执行(权限确认时)就被中断:所有调用都缺响应
        let mut msgs = vec![
            asst_with_calls(&["a", "b"]),
            ChatMessage::user("接着干"),
        ];
        let fixed = super::repair_tool_chain(&mut msgs);
        assert_eq!(fixed, 2);
        let fp = fingerprint(&msgs);
        assert_eq!(
            fp,
            vec![
                ("assistant".into(), "a+b".into()),
                ("tool".into(), "a".into()),
                ("tool".into(), "b".into()),
                ("user".into(), "接着干".into()),
            ]
        );
    }

    #[test]
    fn repair_fills_trailing_declaration() {
        // 历史最后一条是带 tool_calls 的 assistant(中断后立刻退出)
        let mut msgs = vec![asst_with_calls(&["x"])];
        let fixed = super::repair_tool_chain(&mut msgs);
        assert_eq!(fixed, 1);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1].role, Role::Tool);
        assert_eq!(msgs[1].tool_call_id.as_deref(), Some("x"));
    }

    #[test]
    fn repair_leaves_healthy_history_untouched() {
        let mut msgs = vec![
            ChatMessage::user("列目录"),
            asst_with_calls(&["a"]),
            ChatMessage::tool("a", "done"),
            assistant("完事了"),
        ];
        let before = fingerprint(&msgs);
        let fixed = super::repair_tool_chain(&mut msgs);
        assert_eq!(fixed, 0, "完整序列不应动");
        assert_eq!(fingerprint(&msgs), before);
    }

    #[test]
    fn repair_drops_ownerless_tool_message() {
        // 声明方已不在的无主 tool 消息:留着端点一样拒
        let mut msgs = vec![
            ChatMessage::user("hi"),
            ChatMessage::tool("orphan", "没人认领"),
            assistant("完事了"),
        ];
        let fixed = super::repair_tool_chain(&mut msgs);
        assert_eq!(fixed, 0);
        assert!(
            msgs.iter().all(|m| m.role != Role::Tool),
            "无主 tool 消息应被丢弃"
        );
    }
}
