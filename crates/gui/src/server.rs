//! 本地服务:内嵌页面 + WebSocket 事件流。
//!
//! 安全边界(本地服务最怕的两件事):
//! - **越权访问**:只绑 `127.0.0.1`;所有入口要一次性 token(启动时随机、随 URL 打印,
//!   首次 `GET /` 后写入 Cookie 方便刷新);前端的 WebSocket 也带同一 token。
//! - **DNS rebinding / 跨站连 WS**:校验 `Host` 必须是本机地址,`Origin`(浏览器一定会带)
//!   必须同源;不带 `Origin` 的只可能是本机原生客户端,放行。

use std::io::Write;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use tokio::sync::broadcast;
use tokio::sync::mpsc::UnboundedSender;

use crate::bridge::{self, ClientMsg, Hello, LiveState, MemoryForm, ModelForm, NetworkForm};

const INDEX_HTML: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/web/index.html"));
const APP_CSS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/web/app.css"));
const APP_JS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/web/app.js"));
const FAVICON: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/web/favicon.svg"));

macro_rules! vendor {
    ($name:literal, $mime:literal) => {
        (
            concat!("/assets/vendor/", $name),
            $mime,
            include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/web/vendor/", $name)),
        )
    };
}

/// 内嵌资源表(路径, MIME, 内容)。页面、样式、脚本、vendored 库都在这里;
/// 没有目录遍历,也不读磁盘 —— 前端完全来自二进制。
const ASSETS: &[(&str, &str, &str)] = &[
    ("/assets/app.css", "text/css; charset=utf-8", APP_CSS),
    ("/assets/app.js", "text/javascript; charset=utf-8", APP_JS),
    ("/assets/favicon.svg", "image/svg+xml", FAVICON),
    vendor!("marked.min.js", "text/javascript; charset=utf-8"),
    vendor!("purify.min.js", "text/javascript; charset=utf-8"),
    vendor!("highlight.min.js", "text/javascript; charset=utf-8"),
    vendor!("ansi_up.js", "text/javascript; charset=utf-8"),
    vendor!("hljs-github-dark.min.css", "text/css; charset=utf-8"),
    vendor!("hljs-github.min.css", "text/css; charset=utf-8"),
];

/// 页面级 CSP:模型/工具输出是不可信输入,就算净化漏了也不许它发请求或执行脚本。
/// `ws://127.0.0.1:*` 单独列出:部分浏览器不把 `'self'` 扩展到 WebSocket scheme。
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
img-src 'self' data:; font-src 'self'; connect-src 'self' ws://127.0.0.1:* ws://localhost:*; \
base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// 页面 → 宿主的动作(由 `main` 翻译成 `AgentCmd` / `answer_*`)。
#[derive(Debug)]
pub enum UiInput {
    Prompt(String),
    Queue(String),
    Cancel,
    /// 回合结束后取一条排队消息作为新回合
    Drain,
    /// slash 命令(语义在 host::command)
    Command(String),
    /// 切换工作目录
    SetCwd(String),
    /// @ 补全候选
    ListPaths(String),
    /// 系统目录选择器
    PickCwd,
    /// 新建会话
    NewSession,
    /// 确认清空上下文
    Clear,
    /// 撤回一条排队消息(按文本匹配第一条)
    Unqueue(String),
    /// 切换权限档 / 人格 / 服务商(顶栏控件)
    SetMode(String),
    SetPersona(String),
    SetProvider(String),
    /// 压缩上下文(ctx 条旁按钮)
    Compact,
    /// 请求/执行 undo
    UndoList,
    Undo(usize),
    /// 侧边栏:恢复 / 删除 / 备注
    ResumeSession(String),
    DeleteSession(String),
    RenameSession {
        id: String,
        note: String,
    },
    /// 设置页
    OpenSettings,
    SaveModel(ModelForm),
    SaveNetwork(NetworkForm),
    ProbeModels {
        base_url: String,
        api_key: String,
    },
    SavePersonaFile {
        name: String,
        body: String,
    },
    SkillGet(String),
    SkillSave {
        name: String,
        body: String,
    },
    SkillRun {
        name: String,
        args: String,
    },
    MemoryGet {
        filename: String,
        scope: String,
    },
    MemorySave(MemoryForm),
    MemoryDelete {
        filename: String,
        scope: String,
    },
    McpSave(String),
    UpdateCheck,
    UpdateInstall,
    Permission {
        allow: bool,
        always: bool,
    },
    Rounds {
        cont: bool,
    },
}

pub struct AppState {
    pub ui: UnboundedSender<UiInput>,
    /// 下行帧总线(所有已连接页面共享;通常只有一个)
    pub events: broadcast::Sender<String>,
    pub token: String,
    pub port: u16,
    pub hello: Hello,
    pub live: Arc<Mutex<LiveState>>,
    /// 当前连接的页面数(最后一个断开时执行失败关闭,见 D2)
    pub clients: Arc<std::sync::atomic::AtomicUsize>,
}

/// 起服务:先绑端口(拿真实端口回填给握手快照),再交给 axum。
/// `build` 在端口已知后拿到状态 —— 端口可能是 `--port 0` 由系统分配。
pub async fn serve(
    port: u16,
    build: impl FnOnce(u16) -> Arc<AppState>,
) -> anyhow::Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "端口 {port} 绑定失败:{e}。换一个端口(--port 0 让系统分配),或先关掉占用它的进程。"
            )
        })?;
    let addr = listener.local_addr()?;
    let state = build(addr.port());
    let app = router(state);
    let handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            // 不用 eprintln!:GUI 子系统下没有控制台,写失败会 panic
            let _ = writeln!(std::io::stderr(), "⚠ 本地服务退出:{e}");
        }
    });
    Ok((addr, handle))
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/ws", get(ws))
        .route("/assets/*path", get(asset))
        .route("/healthz", get(healthz))
        .with_state(state)
}

#[derive(Debug, Deserialize)]
struct TokenQuery {
    #[serde(default)]
    token: Option<String>,
}

async fn index(
    State(st): State<Arc<AppState>>,
    Query(q): Query<TokenQuery>,
    headers: HeaderMap,
) -> Response {
    if !st.authorized(q.token.as_deref(), &headers) {
        return forbidden("缺少或错误的访问令牌。请用 znaide-gui 启动时打印的完整链接打开。");
    }
    // Cookie 只为本机刷新服务,不含任何权限;SameSite=Strict 挡住跨站请求带上它
    let cookie = format!("znaide_token={}; Path=/; SameSite=Strict", st.token);
    (
        [
            (header::SET_COOKIE, cookie),
            (header::CACHE_CONTROL, "no-store".to_string()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
            (header::CONTENT_SECURITY_POLICY, CSP.to_string()),
        ],
        Html(INDEX_HTML),
    )
        .into_response()
}

/// 本机健康检查(原生窗口/打包冒烟用):只报是否起来、连了几个页面,不含 token。
async fn healthz(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let clients = st.clients.load(std::sync::atomic::Ordering::SeqCst);
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        format!("{{\"ok\":true,\"clients\":{clients}}}"),
    )
}

/// 只发白名单里的内嵌资源:路径来自 URL,但不做任何拼接/磁盘读取。
async fn asset(Path(path): Path<String>) -> Response {
    let key = format!("/assets/{path}");
    match ASSETS.iter().find(|(p, _, _)| *p == key) {
        Some((_, mime, body)) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, (*mime).to_string()),
                (header::CACHE_CONTROL, "no-store".to_string()),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
            ],
            *body,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "asset not found").into_response(),
    }
}

async fn ws(
    State(st): State<Arc<AppState>>,
    Query(q): Query<TokenQuery>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !st.authorized(q.token.as_deref(), &headers) {
        return forbidden("访问令牌不对。");
    }
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !host_is_local(host, st.port) {
        return forbidden("Host 不是本机地址,拒绝连接。");
    }
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    if !origin_ok(origin, st.port) {
        return forbidden("来源(Origin)不是本机页面,拒绝连接。");
    }
    upgrade.on_upgrade(move |socket| client_loop(socket, st))
}

async fn client_loop(socket: WebSocket, st: Arc<AppState>) {
    st.clients.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    serve_client(socket, st.clone()).await;
    // 最后一个页面断开:未决的询问按拒绝处理(D2 失败关闭),别让回合悬着
    if st.clients.fetch_sub(1, std::sync::atomic::Ordering::SeqCst) == 1 {
        abandon_pending(&st);
    }
}

async fn serve_client(mut socket: WebSocket, st: Arc<AppState>) {
    let mut rx = st.events.subscribe();
    // 握手快照:刷新/重连后直接恢复会话 id、提示与未决弹窗
    let hello = {
        let live = st.live.lock().unwrap();
        bridge::hello_frame(&st.hello, &live)
    };
    if socket.send(Message::Text(hello)).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(frame) => {
                    if socket.send(Message::Text(frame)).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    let msg = bridge::error_frame(format!(
                        "事件太快,丢了 {n} 条一次性输出;刷新页面可拿到状态快照。"
                    ));
                    if socket.send(Message::Text(msg)).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Text(text))) => match bridge::parse_client(&text) {
                    Ok(m) => dispatch(m, &st),
                    Err(e) => {
                        let _ = socket.send(Message::Text(bridge::error_frame(e))).await;
                    }
                },
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(_)) => break,
            },
        }
    }
}

fn dispatch(msg: ClientMsg, st: &AppState) {
    // 回答到达即认为询问结束(宿主那边同样只会有一个待决项)
    if matches!(msg, ClientMsg::Permission { .. } | ClientMsg::Rounds { .. }) {
        st.live.lock().unwrap().pending_ask = None;
    }
    let input = match msg {
        ClientMsg::Prompt(t) => UiInput::Prompt(t),
        ClientMsg::Queue(t) => UiInput::Queue(t),
        ClientMsg::Cancel => UiInput::Cancel,
        ClientMsg::Drain => UiInput::Drain,
        ClientMsg::Command(line) => UiInput::Command(line),
        ClientMsg::SetCwd(path) => UiInput::SetCwd(path),
        ClientMsg::ListPaths(prefix) => UiInput::ListPaths(prefix),
        ClientMsg::PickCwd => UiInput::PickCwd,
        ClientMsg::NewSession => UiInput::NewSession,
        ClientMsg::Clear => UiInput::Clear,
        ClientMsg::Unqueue(text) => UiInput::Unqueue(text),
        ClientMsg::SetMode(mode) => UiInput::SetMode(mode),
        ClientMsg::SetPersona(name) => UiInput::SetPersona(name),
        ClientMsg::SetProvider(name) => UiInput::SetProvider(name),
        ClientMsg::Compact => UiInput::Compact,
        ClientMsg::UndoList => UiInput::UndoList,
        ClientMsg::Undo(index) => UiInput::Undo(index),
        ClientMsg::ResumeSession(id) => UiInput::ResumeSession(id),
        ClientMsg::DeleteSession(id) => UiInput::DeleteSession(id),
        ClientMsg::RenameSession { id, note } => UiInput::RenameSession { id, note },
        ClientMsg::OpenSettings => UiInput::OpenSettings,
        ClientMsg::SaveModel(form) => UiInput::SaveModel(form),
        ClientMsg::SaveNetwork(form) => UiInput::SaveNetwork(form),
        ClientMsg::ProbeModels { base_url, api_key } => UiInput::ProbeModels { base_url, api_key },
        ClientMsg::SavePersonaFile { name, body } => UiInput::SavePersonaFile { name, body },
        ClientMsg::SkillGet(name) => UiInput::SkillGet(name),
        ClientMsg::SkillSave { name, body } => UiInput::SkillSave { name, body },
        ClientMsg::SkillRun { name, args } => UiInput::SkillRun { name, args },
        ClientMsg::MemoryGet { filename, scope } => UiInput::MemoryGet { filename, scope },
        ClientMsg::MemorySave(form) => UiInput::MemorySave(form),
        ClientMsg::MemoryDelete { filename, scope } => UiInput::MemoryDelete { filename, scope },
        ClientMsg::McpSave(text) => UiInput::McpSave(text),
        ClientMsg::UpdateCheck => UiInput::UpdateCheck,
        ClientMsg::UpdateInstall => UiInput::UpdateInstall,
        ClientMsg::Permission { allow, always } => UiInput::Permission { allow, always },
        ClientMsg::Rounds { cont } => UiInput::Rounds { cont },
    };
    if st.ui.send(input).is_err() {
        let _ = st
            .events
            .send(bridge::error_frame("引擎已退出,这条动作没有送达。"));
    }
}

/// 最后一个页面断开:未决询问按拒绝回答(失败关闭)。回合因此继续/收尾,
/// 不会一直等一个已经不存在的弹窗。
fn abandon_pending(st: &AppState) {
    let Some(raw) = st.live.lock().unwrap().pending_ask.take() else {
        return;
    };
    let kind = serde_json::from_str::<serde_json::Value>(&raw)
        .ok()
        .and_then(|v| v["kind"].as_str().map(str::to_string));
    let input = match kind.as_deref() {
        Some("permission") => UiInput::Permission {
            allow: false,
            always: false,
        },
        Some("rounds") => UiInput::Rounds { cont: false },
        _ => return,
    };
    let _ = st.ui.send(input);
}

impl AppState {
    /// 入口鉴权:URL 上的 `?token=` 或首次访问后写入的 Cookie,二者其一即可。
    pub fn authorized(&self, token: Option<&str>, headers: &HeaderMap) -> bool {
        if let Some(t) = token.filter(|t| !t.is_empty()) {
            return ct_eq(t, &self.token);
        }
        cookie_token(headers)
            .map(|t| ct_eq(&t, &self.token))
            .unwrap_or(false)
    }
}

fn cookie_token(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';')
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == "znaide_token")
        .map(|(_, v)| v.to_string())
}

/// 定长比较:token 是本地服务的唯一门槛,不做长度提前返回之外的短路。
fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `Host` 必须是本机 + 本服务端口(DNS rebinding 把域名指到 127.0.0.1 时,Host 仍是域名)。
pub fn host_is_local(host: &str, port: u16) -> bool {
    let host = host.trim();
    host == format!("127.0.0.1:{port}") || host == format!("localhost:{port}")
}

/// 浏览器的 WS 一定带 `Origin`;它必须与本机页面同源。没有 `Origin` 的按原生客户端处理。
pub fn origin_ok(origin: Option<&str>, port: u16) -> bool {
    let Some(origin) = origin else {
        return true;
    };
    let Some((scheme, rest)) = origin.split_once("://") else {
        return false;
    };
    if scheme != "http" && scheme != "https" {
        return false;
    }
    host_is_local(rest.trim_end_matches('/'), port)
}

fn forbidden(msg: &str) -> Response {
    (StatusCode::FORBIDDEN, msg.to_string()).into_response()
}

/// 启动时生成的一次性 token(24 字节随机 → 48 位十六进制)。
pub fn new_token() -> String {
    let mut buf = [0u8; 24];
    if getrandom::getrandom(&mut buf).is_err() {
        // 极端情况下(无系统熵源)退化成"时间 + pid",仍然不可猜测到可用的程度
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let pid = std::process::id() as u128;
        let mix = nanos.wrapping_mul(0x9e3779b97f4a7c15).wrapping_add(pid);
        buf[..16].copy_from_slice(&mix.to_le_bytes());
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_must_match_exactly() {
        let st = AppState {
            ui: tokio::sync::mpsc::unbounded_channel().0,
            events: broadcast::channel(4).0,
            token: "abc123".into(),
            port: 7645,
            hello: Hello {
                version: "0".into(),
                product: String::new(),
                gui_version: String::new(),
                engine_version: String::new(),
                model: String::new(),
                provider: String::new(),
                cwd: String::new(),
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
            },
            live: Arc::default(),
            clients: Arc::default(),
        };
        let empty = HeaderMap::new();
        assert!(st.authorized(Some("abc123"), &empty));
        assert!(!st.authorized(Some("abc124"), &empty));
        assert!(!st.authorized(Some(""), &empty));
        assert!(!st.authorized(None, &empty));
    }

    #[test]
    fn cookie_token_is_accepted_for_refresh() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            "other=1; znaide_token=tok; x=2".parse().unwrap(),
        );
        assert_eq!(cookie_token(&headers).as_deref(), Some("tok"));
    }

    #[test]
    fn host_and_origin_must_be_loopback() {
        assert!(host_is_local("127.0.0.1:7645", 7645));
        assert!(host_is_local("localhost:7645", 7645));
        assert!(!host_is_local("evil.example:7645", 7645));
        assert!(!host_is_local("127.0.0.1:9999", 7645));

        assert!(origin_ok(Some("http://127.0.0.1:7645"), 7645));
        assert!(origin_ok(Some("http://localhost:7645/"), 7645));
        assert!(!origin_ok(Some("http://evil.example"), 7645));
        assert!(!origin_ok(Some("null"), 7645));
        // 原生 WS 客户端不带 Origin
        assert!(origin_ok(None, 7645));
    }

    #[test]
    fn token_is_long_and_unique() {
        let a = new_token();
        let b = new_token();
        assert_eq!(a.len(), 48);
        assert_ne!(a, b);
    }
}
