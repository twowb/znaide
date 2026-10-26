//! 桥:宿主事件 → 前端 JSON 帧;前端动作 → 宿主动作。
//!
//! 这一层**只做投影**:不新增能力、不改语义。`SessionEvent` 的每个变体对应一个
//! `kind`,前端据此渲染;两类询问([`Ask`])单独走 `ask` 帧 —— 回答仍由宿主校验
//! (`allow_always` 与入口二次校验),桥不参与判断。

use serde::Deserialize;
use serde_json::{json, Value};

use znaide_core::llm::ChatMessage;
use znaide_core::session::SessionEvent;
use znaide_host::agent::HostEvent;
use znaide_host::ask::Ask;
use znaide_host::NoteLevel;

/// 侧边栏里的一条会话。
#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct SessionFrame {
    pub id: String,
    pub title: String,
    pub note: Option<String>,
    pub headless: bool,
    pub cwd: Option<String>,
    pub mtime_ms: u64,
    pub user_msgs: usize,
    pub assistant_msgs: usize,
}

/// 命令面板里的一个技能(点选后填 `/名字 `)。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SkillFrame {
    pub name: String,
    pub desc: String,
}

/// 顶栏模型/服务商下拉的一项。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProviderFrame {
    pub name: String,
    pub model: String,
}

/// undo 弹层里的一条快照。
#[derive(Debug, Clone, serde::Serialize)]
pub struct UndoFrame {
    pub index: usize,
    pub session: String,
    pub orig: String,
    pub action: String,
    pub ts_ms: u64,
    pub external: bool,
}

/// 读一遍历史会话(标题/消息数/时间),侧边栏与握手快照共用。
pub fn session_frames() -> Vec<SessionFrame> {
    znaide_core::session::list_history_sessions_detailed()
        .into_iter()
        .map(|h| {
            let id = h
                .path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let sum = znaide_core::session::summarize_history(&h.path);
            SessionFrame {
                id,
                title: sum.title,
                note: h.note,
                headless: h.headless,
                cwd: h.cwd.map(|p| p.display().to_string()),
                mtime_ms: sum.mtime_ms,
                user_msgs: sum.user_msgs,
                assistant_msgs: sum.assistant_msgs,
            }
        })
        .collect()
}

/// 页面连上后服务端补发的快照(刷新/重连不丢状态)。
#[derive(Debug, Clone, serde::Serialize)]
pub struct Hello {
    pub version: String,
    pub product: String,
    pub gui_version: String,
    pub engine_version: String,
    pub model: String,
    pub provider: String,
    pub cwd: String,
    /// 机器可读的权限档(ask / acceptEdits / bypassPermissions / yolo)
    pub mode: String,
    /// 界面标签(询问 / 编辑放行 / 全自动 / 超级)
    pub mode_label: String,
    pub persona: String,
    pub first_run: bool,
    pub resolve_error: Option<String>,
    /// 生效上下文窗口(None = 模型不在内置表且配置没写,前端只报绝对量)
    pub ctx_window: Option<usize>,
    pub resume: Option<String>,
    pub notes: Vec<NoteFrame>,
    /// 可切换的服务商(名字 + 该预设的模型),顶栏下拉用
    pub providers: Vec<ProviderFrame>,
    /// 已安装人格(用户级 + 内置)
    pub personas: Vec<String>,
    /// 帮助文本(与 /help 同一份,前端用来做帮助面板)
    pub help: String,
    /// 已安装技能(命令面板补全用)
    pub skills: Vec<SkillFrame>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct NoteFrame {
    pub level: &'static str,
    pub text: String,
}

impl From<&znaide_host::Note> for NoteFrame {
    fn from(n: &znaide_host::Note) -> Self {
        Self {
            level: match n.level {
                NoteLevel::Warn => "warn",
                NoteLevel::Info => "info",
            },
            text: n.text.clone(),
        }
    }
}

/// 顶栏会变的那组状态(权限档/模型/服务商/人格/窗口)。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Status {
    pub model: String,
    pub provider: String,
    /// 运行中实际使用的端点(可能来自命令行覆盖,不一定在 config 里)
    pub base_url: String,
    /// 运行中是否已有 key(只报有无,不回传明文)
    pub api_key_set: bool,
    /// 是否已完成任一来源的配置(首启 = false)
    pub configured: bool,
    pub mode: String,
    pub mode_label: String,
    pub persona: String,
    pub ctx_window: Option<usize>,
}

/// 服务端记录的会话级状态(重连时补发;不参与引擎判断)。
#[derive(Debug, Default)]
pub struct LiveState {
    pub session_id: Option<String>,
    /// 最近一条尚未回答的询问(原始帧;回答后由服务端清空)
    pub pending_ask: Option<String>,
    /// 侧边栏快照(回合结束/增删会话后刷新)
    pub sessions: Vec<SessionFrame>,
    /// 顶栏被改过的状态(未改过则用 hello 里的启动值)
    pub status: Option<Status>,
    /// 运行中切换后的工作目录(重连补发)
    pub cwd: Option<String>,
    /// 启动时静默探测到的新版本说明
    pub update_notice: Option<String>,
}

/// 设置页里的一条请求头。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, serde::Serialize)]
pub struct HeaderPair {
    pub name: String,
    #[serde(default)]
    pub value: String,
}

/// 前端发来的动作。
#[derive(Debug, PartialEq, Eq)]
pub enum ClientMsg {
    /// 新回合(空闲时)
    Prompt(String),
    /// 工作中排队(下一个轮边界插入对话)
    Queue(String),
    /// 中断当前回合
    Cancel,
    /// 回合结束后取一条排队消息当作新回合(等价 TUI 空闲时从 inbox 取)
    Drain,
    /// 执行一条 slash 命令(语义在 host::command,前端只负责送进来)
    Command(String),
    /// 切换工作目录
    SetCwd(String),
    /// @ 补全:列当前 cwd 下的一层路径
    ListPaths(String),
    /// 系统目录选择器
    PickCwd,
    /// 新建会话(换身份、清空界面;保留模型/人格/权限档)
    NewSession,
    /// 确认清空上下文(对应 `/clear` 的确认框)
    Clear,
    /// 从队列里撤回一条(按文本匹配第一条)
    Unqueue(String),
    /// 切换权限档(值同 `Mode::parse`)
    SetMode(String),
    /// 切换人格(空 = 关闭)
    SetPersona(String),
    /// 切换服务商预设(本次运行生效)
    SetProvider(String),
    /// 压缩上下文
    Compact,
    /// 请求 undo 快照列表
    UndoList,
    /// 回滚某条快照(1 起)
    Undo(usize),
    /// 恢复历史会话(按 id)
    ResumeSession(String),
    /// 删除历史会话(按 id)
    DeleteSession(String),
    /// 给会话写备注("改名"),空 = 清除
    RenameSession {
        id: String,
        note: String,
    },
    /// 打开设置页(取快照)
    OpenSettings,
    /// 保存服务商/模型/端点/key/窗口/轮数
    SaveModel(ModelForm),
    /// 保存网络与请求(代理/重试/请求头)
    SaveNetwork(NetworkForm),
    /// 探测端点可用模型(向导/设置页)
    ProbeModels {
        base_url: String,
        api_key: String,
    },
    /// 新建/修改自定义人格
    SavePersonaFile {
        name: String,
        body: String,
    },
    /// 读/存技能正文
    SkillGet(String),
    SkillSave {
        name: String,
        body: String,
    },
    /// 运行技能
    SkillRun {
        name: String,
        args: String,
    },
    /// 读/存/删记忆
    MemoryGet {
        filename: String,
        scope: String,
    },
    MemorySave(MemoryForm),
    MemoryDelete {
        filename: String,
        scope: String,
    },
    /// 保存 mcp.json 原文
    McpSave(String),
    /// 关于与更新
    UpdateCheck,
    UpdateInstall,
    /// 回答权限询问
    Permission {
        allow: bool,
        always: bool,
    },
    /// 回答轮数续跑询问
    Rounds {
        cont: bool,
    },
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
pub struct ModelForm {
    pub provider: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub clear_key: bool,
    /// 0 = 自动(交给内置表)
    #[serde(default)]
    pub context_window: usize,
    /// 空字符串 = 不设置(默认 200)
    #[serde(default)]
    pub max_turns: Option<usize>,
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
pub struct NetworkForm {
    /// auto / direct / manual
    pub mode: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub retry: usize,
    #[serde(default = "default_true")]
    pub headers_enabled: bool,
    #[serde(default)]
    pub headers: Vec<HeaderPair>,
    /// 当前服务商条目里的额外请求头(叠加在顶层之上)
    #[serde(default)]
    pub provider_headers: Vec<HeaderPair>,
    /// None = 继承顶层;Some(false) = 这家强制不发
    #[serde(default)]
    pub provider_headers_enabled: Option<bool>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
pub struct MemoryForm {
    #[serde(default)]
    pub filename: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub mtype: String,
    /// global / project
    pub scope: String,
    #[serde(default)]
    pub body: String,
}

/// 上行帧的线上形态:`{"type":"prompt","text":"…"}` 等(见 `plan-gui.md` 的协议表)。
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Wire {
    Prompt {
        text: String,
    },
    Queue {
        text: String,
    },
    Cancel,
    Drain,
    Command {
        line: String,
    },
    SetCwd {
        path: String,
    },
    ListPaths {
        prefix: String,
    },
    PickCwd,
    NewSession,
    Clear,
    Unqueue {
        text: String,
    },
    Mode {
        mode: String,
    },
    Persona {
        name: String,
    },
    Provider {
        name: String,
    },
    Compact,
    UndoList,
    Undo {
        index: usize,
    },
    Resume {
        id: String,
    },
    DeleteSession {
        id: String,
    },
    RenameSession {
        id: String,
        note: String,
    },
    SettingsOpen,
    SaveModel {
        #[serde(flatten)]
        form: ModelForm,
    },
    SaveNetwork {
        #[serde(flatten)]
        form: NetworkForm,
    },
    ProbeModels {
        base_url: String,
        #[serde(default)]
        api_key: String,
    },
    SavePersonaFile {
        name: String,
        body: String,
    },
    SkillGet {
        name: String,
    },
    SkillSave {
        name: String,
        body: String,
    },
    SkillRun {
        name: String,
        #[serde(default)]
        args: String,
    },
    MemoryGet {
        filename: String,
        scope: String,
    },
    MemorySave {
        #[serde(flatten)]
        form: MemoryForm,
    },
    MemoryDelete {
        filename: String,
        scope: String,
    },
    McpSave {
        text: String,
    },
    UpdateCheck,
    UpdateInstall,
    Permission {
        allow: bool,
        #[serde(default)]
        always: bool,
    },
    Rounds {
        cont: bool,
    },
}

/// 解析上行帧。错误文案直接回给前端(不 panic:对面可能是任何东西)。
pub fn parse_client(text: &str) -> Result<ClientMsg, String> {
    let wire: Wire = serde_json::from_str(text).map_err(|e| format!("无法解析的消息:{e}"))?;
    Ok(match wire {
        Wire::Prompt { text } if text.trim().is_empty() => return Err("消息不能为空".into()),
        Wire::Prompt { text } => ClientMsg::Prompt(text),
        Wire::Queue { text } if text.trim().is_empty() => return Err("消息不能为空".into()),
        Wire::Queue { text } => ClientMsg::Queue(text),
        Wire::Cancel => ClientMsg::Cancel,
        Wire::Drain => ClientMsg::Drain,
        Wire::Command { line } if line.trim().is_empty() => return Err("命令不能为空".into()),
        Wire::Command { line } => ClientMsg::Command(line),
        Wire::SetCwd { path } => ClientMsg::SetCwd(path),
        Wire::ListPaths { prefix } => ClientMsg::ListPaths(prefix),
        Wire::PickCwd => ClientMsg::PickCwd,
        Wire::NewSession => ClientMsg::NewSession,
        Wire::Clear => ClientMsg::Clear,
        Wire::Unqueue { text } => ClientMsg::Unqueue(text),
        Wire::Mode { mode } => ClientMsg::SetMode(mode),
        Wire::Persona { name } => ClientMsg::SetPersona(name),
        Wire::Provider { name } => ClientMsg::SetProvider(name),
        Wire::Compact => ClientMsg::Compact,
        Wire::UndoList => ClientMsg::UndoList,
        Wire::Undo { index } => ClientMsg::Undo(index),
        Wire::Resume { id } => ClientMsg::ResumeSession(id),
        Wire::DeleteSession { id } => ClientMsg::DeleteSession(id),
        Wire::RenameSession { id, note } => ClientMsg::RenameSession { id, note },
        Wire::SettingsOpen => ClientMsg::OpenSettings,
        Wire::SaveModel { form } => ClientMsg::SaveModel(form),
        Wire::SaveNetwork { form } => ClientMsg::SaveNetwork(form),
        Wire::ProbeModels { base_url, api_key } => ClientMsg::ProbeModels { base_url, api_key },
        Wire::SavePersonaFile { name, body } => ClientMsg::SavePersonaFile { name, body },
        Wire::SkillGet { name } => ClientMsg::SkillGet(name),
        Wire::SkillSave { name, body } => ClientMsg::SkillSave { name, body },
        Wire::SkillRun { name, args } => ClientMsg::SkillRun { name, args },
        Wire::MemoryGet { filename, scope } => ClientMsg::MemoryGet { filename, scope },
        Wire::MemorySave { form } => ClientMsg::MemorySave(form),
        Wire::MemoryDelete { filename, scope } => ClientMsg::MemoryDelete { filename, scope },
        Wire::McpSave { text } => ClientMsg::McpSave(text),
        Wire::UpdateCheck => ClientMsg::UpdateCheck,
        Wire::UpdateInstall => ClientMsg::UpdateInstall,
        Wire::Permission { allow, always } => ClientMsg::Permission { allow, always },
        Wire::Rounds { cont } => ClientMsg::Rounds { cont },
    })
}

/// 宿主事件 → 下行帧。返回 `None` = 这条事件不投影(被 host 截住的询问)。
pub fn frame(ev: &HostEvent) -> Option<String> {
    let v = match ev {
        HostEvent::Ask(Ask::Permission(p)) => json!({
            "type": "ask",
            "kind": "permission",
            "title": p.title,
            "body": p.body,
            "warning": p.warning,
            "allow_always": p.allow_always,
        }),
        HostEvent::Ask(Ask::Rounds(r)) => json!({
            "type": "ask",
            "kind": "rounds",
            "used": r.used,
        }),
        HostEvent::Session(ev) => return session_frame(ev).map(|v| v.to_string()),
    };
    Some(v.to_string())
}

fn session_frame(ev: &SessionEvent) -> Option<Value> {
    Some(match ev {
        SessionEvent::TurnStarted => json!({"type":"event","kind":"turn_started"}),
        SessionEvent::ReasoningDelta(t) => {
            json!({"type":"event","kind":"reasoning_delta","text":t})
        }
        SessionEvent::TextDelta(t) => json!({"type":"event","kind":"text_delta","text":t}),
        SessionEvent::ToolStarted {
            name,
            args,
            idle_ms,
        } => json!({
            "type":"event","kind":"tool_started",
            "name":name,"args":args,"idle_ms":idle_ms,
        }),
        SessionEvent::ToolFinished { name, ok, output } => json!({
            "type":"event","kind":"tool_finished",
            "name":name,"ok":ok,"output":output,
        }),
        SessionEvent::ToolSilentAlert { name, level } => json!({
            "type":"event","kind":"tool_silent_alert","name":name,"level":level,
        }),
        SessionEvent::ToolOutputDelta { name, delta } => json!({
            "type":"event","kind":"tool_output_delta","name":name,"delta":delta,
        }),
        SessionEvent::PersonaChanged { name } => {
            json!({"type":"event","kind":"persona_changed","name":name})
        }
        SessionEvent::RoundStarted { used, limit } => json!({
            "type":"event","kind":"round_started","used":used,"limit":limit,
        }),
        SessionEvent::UserInjected { text } => {
            json!({"type":"event","kind":"user_injected","text":text})
        }
        SessionEvent::LlmRetrying {
            attempt,
            max,
            reason,
        } => json!({
            "type":"event","kind":"llm_retrying",
            "attempt":attempt,"max":max,"reason":reason,
        }),
        SessionEvent::Notice(t) => json!({"type":"event","kind":"notice","text":t}),
        SessionEvent::Usage { prompt, completion } => json!({
            "type":"event","kind":"usage","prompt":prompt,"completion":completion,
        }),
        SessionEvent::ContextCleared => json!({"type":"event","kind":"context_cleared"}),
        SessionEvent::CompactionStarted => {
            json!({"type":"event","kind":"compaction_started"})
        }
        SessionEvent::ContextCompacted {
            removed,
            summary,
            kept,
        } => json!({
            "type":"event","kind":"context_compacted",
            "removed":removed,"summary":summary,
            "messages":kept.iter().map(msg_json).collect::<Vec<_>>(),
        }),
        SessionEvent::TurnFinished { text, truncated } => json!({
            "type":"event","kind":"turn_finished","text":text,"truncated":truncated,
        }),
        SessionEvent::SessionInfo { id, history } => json!({
            "type":"event","kind":"session_info","id":id,"history":history,
        }),
        SessionEvent::HistoryLoaded { count, messages } => json!({
            "type":"event","kind":"history_loaded","count":count,
            "messages":messages.iter().map(msg_json).collect::<Vec<_>>(),
        }),
        // 询问由 host 截住,永远不会到这里
        SessionEvent::PermissionRequest { .. } | SessionEvent::RoundsExhausted { .. } => {
            return None
        }
    })
}

fn msg_json(m: &ChatMessage) -> Value {
    json!({
        "role": m.role.as_str(),
        "text": m.content.clone().unwrap_or_default(),
        "reasoning": m.reasoning_content.clone().filter(|s| !s.is_empty()),
        "tool_calls": m.tool_calls.as_ref().map(|calls| calls.iter().map(|c| json!({
            "id": c.id,
            "name": c.function.name,
            "args": c.function.arguments,
        })).collect::<Vec<_>>()).unwrap_or_default(),
        "tool_call_id": m.tool_call_id,
    })
}

/// slash 命令的一行输出(notice = 消息流提示,output = 等宽块)。
pub fn command_frame(kind: &'static str, text: &str) -> String {
    json!({"type":"command_line","kind":kind,"text":text}).to_string()
}

/// 需要界面确认的破坏性操作(目前只有 `/clear`)。
pub fn confirm_frame(kind: &'static str, title: &str, body: &str) -> String {
    json!({"type":"confirm","kind":kind,"title":title,"body":body}).to_string()
}

/// 工作目录切换成功
pub fn cwd_frame(path: &str) -> String {
    json!({"type": "cwd", "path": path}).to_string()
}

/// @ 补全候选
pub fn paths_frame(prefix: &str, items: &[(String, bool)], truncated: bool) -> String {
    json!({
        "type": "paths",
        "prefix": prefix,
        "truncated": truncated,
        "items": items.iter().map(|(p, d)| json!({"path": p, "dir": d})).collect::<Vec<_>>(),
    })
    .to_string()
}

/// 目录选择器不可用(前端回退到输入框)
pub fn cwd_pick_failed_frame(message: &str) -> String {
    json!({"type": "cwd_pick_failed", "message": message}).to_string()
}

/// 服务端即将退出:页面收到后停止重连。
pub fn shutdown_frame() -> String {
    json!({"type":"shutdown"}).to_string()
}

/// 握手快照:hello + 会话 id + 未决询问(刷新页面直接恢复弹窗)。
pub fn hello_frame(h: &Hello, live: &LiveState) -> String {
    let mut v = serde_json::to_value(h).expect("hello 序列化不会失败");
    if let Value::Object(map) = &mut v {
        map.insert("type".into(), json!("hello"));
        map.insert("session_id".into(), json!(live.session_id));
        map.insert(
            "pending_ask".into(),
            live.pending_ask
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(Value::Null),
        );
        map.insert(
            "sessions".into(),
            serde_json::to_value(&live.sessions).unwrap_or(Value::Null),
        );
        map.insert(
            "status".into(),
            serde_json::to_value(&live.status).unwrap_or(Value::Null),
        );
        map.insert(
            "cwd".into(),
            live.cwd
                .clone()
                .map(Value::String)
                .unwrap_or_else(|| serde_json::to_value(&h.cwd).unwrap_or(Value::Null)),
        );
        map.insert(
            "update_notice".into(),
            serde_json::to_value(&live.update_notice).unwrap_or(Value::Null),
        );
    }
    v.to_string()
}

/// 顶栏状态(权限档/模型/服务商/人格切换后同步给所有页面)。
pub fn status_frame(s: &Status) -> String {
    let mut v = serde_json::to_value(s).expect("status 序列化不会失败");
    if let Value::Object(map) = &mut v {
        map.insert("type".into(), json!("status"));
    }
    v.to_string()
}

/// 侧边栏会话列表(增删/回合结束后的刷新)。
pub fn sessions_frame(items: &[SessionFrame]) -> String {
    json!({"type":"sessions","items":items}).to_string()
}

/// 恢复会话后让页面更新状态栏/侧栏高亮。
pub fn session_info_frame(id: &str) -> String {
    json!({"type":"event","kind":"session_info","id":id,"history":null}).to_string()
}

/// `/resume` 无参:展开并聚焦会话侧栏(替代 TUI 的全屏窗口)。
pub fn open_sessions_frame() -> String {
    json!({"type":"open_sessions"}).to_string()
}

/// undo 快照列表(前端弹层点选回滚)。
pub fn undo_frame(items: &[UndoFrame]) -> String {
    json!({"type":"undo_list","items":items}).to_string()
}

/// 设置页快照(每次保存后重发,前端整页重填)。
pub fn settings_frame(data: serde_json::Value) -> String {
    json!({"type": "settings", "data": data}).to_string()
}

/// 技能正文(编辑用)。
pub fn skill_content_frame(name: &str, path: &str, body: &str, editable: bool) -> String {
    json!({
        "type": "skill_content",
        "name": name,
        "path": path,
        "body": body,
        "editable": editable,
    })
    .to_string()
}

/// 记忆正文(查看/编辑用)。
pub fn memory_content_frame(
    filename: &str,
    scope: &str,
    name: &str,
    description: &str,
    mtype: &str,
    body: &str,
) -> String {
    json!({
        "type": "memory_content",
        "filename": filename,
        "scope": scope,
        "name": name,
        "description": description,
        "mtype": mtype,
        "body": body,
    })
    .to_string()
}

/// 模型探测结果(向导里选模型)。
pub fn model_list_frame(models: &[String]) -> String {
    json!({"type": "model_list", "models": models}).to_string()
}

/// 配置完成(首启向导保存并可解析):前端关掉设置页回到对话。
pub fn configured_frame() -> String {
    json!({"type": "configured"}).to_string()
}

/// 人格列表变化(设置页新建/编辑后刷新顶栏下拉)。
pub fn personas_frame(items: &[String]) -> String {
    json!({"type": "personas", "items": items}).to_string()
}

/// 更新结果(检查/安装)。
pub fn update_result_frame(kind: &str, text: &str) -> String {
    json!({"type": "update_result", "kind": kind, "text": text}).to_string()
}

/// 启动时静默探测到新版本:顶栏提示点。
pub fn update_available_frame(text: &str) -> String {
    json!({"type": "update_available", "text": text}).to_string()
}

/// 前端可展示的错误帧(服务端自检失败 / 动作没有接收者时用)。
pub fn error_frame(message: impl Into<String>) -> String {
    json!({"type":"error","message":message.into()}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_frame(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn text_delta_projects_to_event() {
        let frame = frame(&HostEvent::Session(SessionEvent::TextDelta("你好".into()))).unwrap();
        let v = parse_frame(&frame);
        assert_eq!(v["type"], "event");
        assert_eq!(v["kind"], "text_delta");
        assert_eq!(v["text"], "你好");
    }

    #[test]
    fn tool_started_keeps_args_and_budget() {
        let frame = frame(&HostEvent::Session(SessionEvent::ToolStarted {
            name: "run_shell_command".into(),
            args: "cargo test".into(),
            idle_ms: Some(90_000),
        }))
        .unwrap();
        let v = parse_frame(&frame);
        assert_eq!(v["kind"], "tool_started");
        assert_eq!(v["args"], "cargo test");
        assert_eq!(v["idle_ms"], 90_000);
    }

    #[test]
    fn permission_ask_carries_allow_always_flag() {
        let frame = frame(&HostEvent::Ask(Ask::Permission(
            znaide_host::ask::PermissionAsk {
                title: "执行命令".into(),
                body: "rm -rf /tmp/x".into(),
                warning: Some("高危".into()),
                allow_always: false,
            },
        )))
        .unwrap();
        let v = parse_frame(&frame);
        assert_eq!(v["type"], "ask");
        assert_eq!(v["kind"], "permission");
        assert_eq!(v["allow_always"], false);
        assert_eq!(v["warning"], "高危");
    }

    #[test]
    fn intercepted_asks_are_not_projected() {
        // 这两类事件理论上到不了桥;真到了也不能把 oneshot 泄露成 JSON
        let ev = SessionEvent::RoundsExhausted {
            used: 200,
            tx: tokio::sync::oneshot::channel().0,
        };
        assert!(frame(&HostEvent::Session(ev)).is_none());
    }

    #[test]
    fn parses_client_frames() {
        assert_eq!(
            parse_client(r#"{"type":"prompt","text":"hi"}"#).unwrap(),
            ClientMsg::Prompt("hi".into())
        );
        assert_eq!(
            parse_client(r#"{"type":"queue","text":"wait"}"#).unwrap(),
            ClientMsg::Queue("wait".into())
        );
        assert_eq!(
            parse_client(r#"{"type":"permission","allow":true}"#).unwrap(),
            ClientMsg::Permission {
                allow: true,
                always: false
            }
        );
        assert_eq!(
            parse_client(r#"{"type":"rounds","cont":true}"#).unwrap(),
            ClientMsg::Rounds { cont: true }
        );
        assert!(parse_client(r#"{"type":"prompt"}"#).is_err());
        assert!(parse_client(r#"{"type":"prompt","text":"  "}"#).is_err());
        assert!(parse_client("not json").is_err());
    }

    #[test]
    fn parses_command_and_queue_control_frames() {
        assert_eq!(
            parse_client(r#"{"type":"command","line":"/help"}"#).unwrap(),
            ClientMsg::Command("/help".into())
        );
        assert_eq!(
            parse_client(r#"{"type":"clear"}"#).unwrap(),
            ClientMsg::Clear
        );
        assert_eq!(
            parse_client(r#"{"type":"unqueue","text":"排队那条"}"#).unwrap(),
            ClientMsg::Unqueue("排队那条".into())
        );
        assert!(parse_client(r#"{"type":"command","line":"  "}"#).is_err());
    }

    #[test]
    fn command_and_confirm_frames_have_stable_shape() {
        let v = parse_frame(&command_frame("output", "用法"));
        assert_eq!(v["type"], "command_line");
        assert_eq!(v["kind"], "output");
        assert_eq!(v["text"], "用法");

        let v = parse_frame(&confirm_frame("clear", "清空会话?", "不可恢复"));
        assert_eq!(v["type"], "confirm");
        assert_eq!(v["kind"], "clear");
        assert_eq!(v["title"], "清空会话?");

        assert_eq!(parse_frame(&shutdown_frame())["type"], "shutdown");
    }

    #[test]
    fn history_messages_keep_tool_calls_and_ids() {
        let call = znaide_core::llm::types::ToolCall {
            id: "call_1".into(),
            call_type: "function".into(),
            function: znaide_core::llm::types::FunctionCall {
                name: "read_file".into(),
                arguments: r#"{"path":"a"}"#.into(),
            },
        };
        let msgs = vec![
            ChatMessage::assistant_with_tool_calls(None, vec![call]),
            ChatMessage::tool("call_1", "文件内容"),
        ];
        let frame = frame(&HostEvent::Session(SessionEvent::HistoryLoaded {
            count: msgs.len(),
            messages: msgs,
        }))
        .unwrap();
        let v = parse_frame(&frame);
        assert_eq!(v["messages"][0]["tool_calls"][0]["name"], "read_file");
        assert_eq!(v["messages"][0]["tool_calls"][0]["id"], "call_1");
        assert_eq!(v["messages"][1]["tool_call_id"], "call_1");
        assert_eq!(v["messages"][1]["text"], "文件内容");
    }

    #[test]
    fn parses_settings_frames() {
        assert_eq!(
            parse_client(r#"{"type":"settings_open"}"#).unwrap(),
            ClientMsg::OpenSettings
        );
        match parse_client(
            r#"{"type":"save_model","provider":"deepseek","model":"m","base_url":"http://x","api_key":"k","clear_key":true,"context_window":128000,"max_turns":50}"#,
        )
        .unwrap()
        {
            ClientMsg::SaveModel(f) => {
                assert_eq!(f.provider, "deepseek");
                assert_eq!(f.model, "m");
                assert_eq!(f.base_url, "http://x");
                assert_eq!(f.api_key, "k");
                assert!(f.clear_key);
                assert_eq!(f.context_window, 128_000);
                assert_eq!(f.max_turns, Some(50));
            }
            other => panic!("模型表单解析不对:{other:?}"),
        }
        match parse_client(
            r#"{"type":"save_network","mode":"manual","url":"http://127.0.0.1:7897","retry":2,"headers_enabled":false,"headers":[{"name":"x-a","value":"1"}]}"#,
        )
        .unwrap()
        {
            ClientMsg::SaveNetwork(f) => {
                assert_eq!(f.mode, "manual");
                assert_eq!(f.retry, 2);
                assert!(!f.headers_enabled);
                assert_eq!(f.headers.len(), 1);
                assert_eq!(f.headers[0].name, "x-a");
            }
            other => panic!("网络表单解析不对:{other:?}"),
        }
        match parse_client(
            r#"{"type":"memory_save","filename":"","name":"周报","description":"d","mtype":"project","scope":"global","body":"正文"}"#,
        )
        .unwrap()
        {
            ClientMsg::MemorySave(f) => {
                assert_eq!(f.name, "周报");
                assert_eq!(f.scope, "global");
                assert_eq!(f.body, "正文");
            }
            other => panic!("记忆表单解析不对:{other:?}"),
        }
        assert_eq!(
            parse_client(r#"{"type":"skill_run","name":"demo","args":"a b"}"#).unwrap(),
            ClientMsg::SkillRun {
                name: "demo".into(),
                args: "a b".into()
            }
        );
        assert_eq!(
            parse_client(r#"{"type":"mcp_save","text":"{}"}"#).unwrap(),
            ClientMsg::McpSave("{}".into())
        );
        assert_eq!(
            parse_client(r#"{"type":"update_check"}"#).unwrap(),
            ClientMsg::UpdateCheck
        );
        assert_eq!(
            parse_client(r#"{"type":"update_install"}"#).unwrap(),
            ClientMsg::UpdateInstall
        );
        assert_eq!(
            parse_client(r#"{"type":"probe_models","base_url":"http://x","api_key":"k"}"#).unwrap(),
            ClientMsg::ProbeModels {
                base_url: "http://x".into(),
                api_key: "k".into()
            }
        );
    }

    #[test]
    fn settings_result_frames_have_stable_shape() {
        let v = parse_frame(&settings_frame(json!({"model": "m"})));
        assert_eq!(v["type"], "settings");
        assert_eq!(v["data"]["model"], "m");

        let v = parse_frame(&skill_content_frame("demo", "/tmp/SKILL.md", "正文", true));
        assert_eq!(v["type"], "skill_content");
        assert_eq!(v["editable"], true);

        let v = parse_frame(&memory_content_frame(
            "m1", "global", "名", "述", "user", "正文",
        ));
        assert_eq!(v["type"], "memory_content");
        assert_eq!(v["name"], "名");
        assert_eq!(v["scope"], "global");

        let v = parse_frame(&update_result_frame("check", "已是最新"));
        assert_eq!(v["type"], "update_result");
        assert_eq!(v["kind"], "check");

        let v = parse_frame(&update_available_frame("发现新版本"));
        assert_eq!(v["type"], "update_available");

        let v = parse_frame(&personas_frame(&["a".into()]));
        assert_eq!(v["items"][0], "a");

        let v = parse_frame(&model_list_frame(&["m1".into(), "m2".into()]));
        assert_eq!(v["type"], "model_list");
        assert_eq!(v["models"][1], "m2");
        assert_eq!(parse_frame(&configured_frame())["type"], "configured");
    }

    #[test]
    fn parses_control_panel_frames() {
        assert_eq!(
            parse_client(r#"{"type":"mode","mode":"yolo"}"#).unwrap(),
            ClientMsg::SetMode("yolo".into())
        );
        assert_eq!(
            parse_client(r#"{"type":"persona","name":"严谨"}"#).unwrap(),
            ClientMsg::SetPersona("严谨".into())
        );
        assert_eq!(
            parse_client(r#"{"type":"provider","name":"deepseek"}"#).unwrap(),
            ClientMsg::SetProvider("deepseek".into())
        );
        assert_eq!(
            parse_client(r#"{"type":"compact"}"#).unwrap(),
            ClientMsg::Compact
        );
        assert_eq!(
            parse_client(r#"{"type":"undo_list"}"#).unwrap(),
            ClientMsg::UndoList
        );
        assert_eq!(
            parse_client(r#"{"type":"undo","index":3}"#).unwrap(),
            ClientMsg::Undo(3)
        );
        assert_eq!(
            parse_client(r#"{"type":"resume","id":"abc"}"#).unwrap(),
            ClientMsg::ResumeSession("abc".into())
        );
        assert_eq!(
            parse_client(r#"{"type":"delete_session","id":"abc"}"#).unwrap(),
            ClientMsg::DeleteSession("abc".into())
        );
        assert_eq!(
            parse_client(r#"{"type":"rename_session","id":"abc","note":"日报"}"#).unwrap(),
            ClientMsg::RenameSession {
                id: "abc".into(),
                note: "日报".into()
            }
        );
    }

    #[test]
    fn status_sessions_and_undo_frames_have_stable_shape() {
        let v = parse_frame(&status_frame(&Status {
            model: "m".into(),
            provider: "p".into(),
            base_url: "http://x".into(),
            api_key_set: true,
            configured: true,
            mode: "ask".into(),
            mode_label: "询问".into(),
            persona: String::new(),
            ctx_window: Some(128_000),
        }));
        assert_eq!(v["type"], "status");
        assert_eq!(v["model"], "m");
        assert_eq!(v["ctx_window"], 128_000);

        let items = vec![SessionFrame {
            id: "s1".into(),
            title: "第一条".into(),
            note: Some("备注".into()),
            headless: false,
            cwd: None,
            mtime_ms: 42,
            user_msgs: 2,
            assistant_msgs: 1,
        }];
        let v = parse_frame(&sessions_frame(&items));
        assert_eq!(v["type"], "sessions");
        assert_eq!(v["items"][0]["title"], "第一条");
        assert_eq!(v["items"][0]["note"], "备注");

        let v = parse_frame(&undo_frame(&[UndoFrame {
            index: 1,
            session: "s1".into(),
            orig: "/tmp/a.rs".into(),
            action: "edit".into(),
            ts_ms: 9,
            external: false,
        }]));
        assert_eq!(v["type"], "undo_list");
        assert_eq!(v["items"][0]["orig"], "/tmp/a.rs");
        assert_eq!(
            parse_frame(&session_info_frame("s1"))["kind"],
            "session_info"
        );
        assert_eq!(parse_frame(&open_sessions_frame())["type"], "open_sessions");
    }

    #[test]
    fn hello_includes_sessions_and_live_status() {
        let hello = Hello {
            version: "t".into(),
            product: "修凡AI全能助手".into(),
            gui_version: "0.0.1".into(),
            engine_version: "1.1.0".into(),
            model: "m".into(),
            provider: "p".into(),
            cwd: "/tmp".into(),
            mode: "ask".into(),
            mode_label: "询问".into(),
            persona: String::new(),
            first_run: false,
            resolve_error: None,
            ctx_window: None,
            resume: None,
            notes: vec![],
            providers: vec![],
            personas: vec![],
            help: String::new(),
            skills: vec![SkillFrame {
                name: "demo".into(),
                desc: "演示".into(),
            }],
        };
        let live = LiveState {
            sessions: vec![SessionFrame {
                id: "s1".into(),
                ..Default::default()
            }],
            status: Some(Status {
                model: "new".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let v = parse_frame(&hello_frame(&hello, &live));
        assert_eq!(v["sessions"][0]["id"], "s1");
        assert_eq!(v["status"]["model"], "new");
        assert_eq!(v["skills"][0]["name"], "demo");
    }

    #[test]
    fn hello_carries_pending_ask_after_refresh() {
        let hello = Hello {
            version: "1.2.0".into(),
            product: "修凡AI全能助手".into(),
            gui_version: "0.0.1".into(),
            engine_version: "1.1.0".into(),
            model: "m".into(),
            provider: "p".into(),
            cwd: "/tmp".into(),
            mode: "ask".into(),
            mode_label: "询问".into(),
            persona: String::new(),
            first_run: false,
            resolve_error: None,
            ctx_window: None,
            resume: None,
            notes: vec![],
            providers: vec![],
            personas: vec![],
            help: String::new(),
            skills: vec![],
        };
        let ask = frame(&HostEvent::Ask(Ask::Rounds(znaide_host::ask::RoundsAsk {
            used: 200,
        })))
        .unwrap();
        let live = LiveState {
            session_id: Some("abc".into()),
            pending_ask: Some(ask),
            ..Default::default()
        };
        let v = parse_frame(&hello_frame(&hello, &live));
        assert_eq!(v["type"], "hello");
        assert_eq!(v["session_id"], "abc");
        assert_eq!(v["pending_ask"]["kind"], "rounds");
    }
}
