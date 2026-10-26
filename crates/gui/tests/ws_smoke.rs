//! 骨架验收:本地服务起得来、入口有 token 门槛、WS 收得到快照、上行动作到得了宿主。
//! 不碰真模型 —— 事件总线是注入的,权限/轮数政策仍在 host 那边(已有单测)。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use znaide_gui::bridge::{self, Hello, LiveState};
use znaide_gui::server::{self, AppState, UiInput};

const TOKEN: &str = "test-token-0123456789";

/// 测试里直连本机,别被环境里的 HTTP_PROXY 带走
fn http_client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

fn hello() -> Hello {
    Hello {
        version: "test".into(),
        product: "修凡AI全能助手".into(),
        gui_version: "0.0.1".into(),
        engine_version: "1.1.0".into(),
        model: "mock-model".into(),
        provider: "mock".into(),
        cwd: "/tmp".into(),
        mode: "ask".into(),
        mode_label: "询问".into(),
        persona: String::new(),
        first_run: false,
        resolve_error: None,
        ctx_window: Some(128_000),
        resume: None,
        notes: vec![],
        providers: vec![],
        personas: vec![],
        help: String::new(),
        skills: vec![],
    }
}

struct Harness {
    port: u16,
    ui: mpsc::UnboundedReceiver<UiInput>,
    bus: broadcast::Sender<String>,
    live: Arc<Mutex<LiveState>>,
}

async fn start() -> Harness {
    let (ui_tx, ui) = mpsc::unbounded_channel();
    let (bus, _) = broadcast::channel::<String>(64);
    let live: Arc<Mutex<LiveState>> = Arc::default();
    let (addr, _task) = server::serve(0, {
        let bus = bus.clone();
        let live = live.clone();
        move |port| {
            Arc::new(AppState {
                ui: ui_tx.clone(),
                events: bus,
                token: TOKEN.into(),
                port,
                hello: hello(),
                live,
                clients: Arc::default(),
            })
        }
    })
    .await
    .expect("绑定随机端口");
    Harness {
        port: addr.port(),
        ui,
        bus,
        live,
    }
}

#[tokio::test]
async fn page_requires_token_and_sets_cookie() {
    let h = start().await;
    let http = http_client();

    let denied = http
        .get(format!("http://127.0.0.1:{}/", h.port))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 403, "没有 token 不允许进页面");

    let ok = http
        .get(format!("http://127.0.0.1:{}/?token={TOKEN}", h.port))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    let cookie = ok
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        cookie.contains("znaide_token="),
        "首次访问要写 Cookie: {cookie}"
    );
    let body = ok.text().await.unwrap();
    assert!(
        body.contains("修凡") || body.contains("znaide"),
        "内嵌页面应能返回"
    );
}

#[tokio::test]
async fn page_sets_content_security_policy() {
    let h = start().await;
    let r = http_client()
        .get(format!("http://127.0.0.1:{}/?token={TOKEN}", h.port))
        .send()
        .await
        .unwrap();
    let csp = r
        .headers()
        .get("content-security-policy")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(csp.contains("default-src 'none'"), "CSP 缺省应全禁:{csp}");
    assert!(csp.contains("script-src 'self'"), "脚本只许同源:{csp}");
    assert!(csp.contains("connect-src"), "WS 连接需放行:{csp}");
}

#[tokio::test]
async fn healthz_reports_connected_pages() {
    let h = start().await;
    let http = http_client();
    let base = format!("http://127.0.0.1:{}", h.port);
    let before: Value = http
        .get(format!("{base}/healthz"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(before["ok"], true);
    assert_eq!(before["clients"], 0);

    let (mut ws, _) =
        tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{}/ws?token={TOKEN}", h.port))
            .await
            .unwrap();
    let _ = next_json(&mut ws).await;
    let after: Value = http
        .get(format!("{base}/healthz"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(after["clients"], 1);
}

#[tokio::test]
async fn assets_are_served() {
    let h = start().await;
    let http = http_client();
    let base = format!("http://127.0.0.1:{}", h.port);
    // 页面、样式、脚本与 vendored 库都要能从二进制里取到
    let expected = [
        ("/assets/app.css", "css"),
        ("/assets/app.js", "javascript"),
        ("/assets/favicon.svg", "svg"),
        ("/assets/vendor/marked.min.js", "javascript"),
        ("/assets/vendor/purify.min.js", "javascript"),
        ("/assets/vendor/highlight.min.js", "javascript"),
        ("/assets/vendor/ansi_up.js", "javascript"),
        ("/assets/vendor/hljs-github-dark.min.css", "css"),
        ("/assets/vendor/hljs-github.min.css", "css"),
    ];
    for (path, want) in expected {
        let r = http.get(format!("{base}{path}")).send().await.unwrap();
        assert_eq!(r.status(), 200, "{path}");
        let ct = r
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(ct.contains(want), "{path} 的 Content-Type 是 {ct}");
    }
    // 白名单外的一律 404,不做目录读取
    let missing = http
        .get(format!("{base}/assets/vendor/../../Cargo.toml"))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);
}

#[tokio::test]
async fn ws_handshakes_and_roundtrips_frames() {
    let mut h = start().await;
    let ws_url = format!("ws://127.0.0.1:{}/ws?token={TOKEN}", h.port);
    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url).await.unwrap();

    // 1) 握手快照
    let first = next_json(&mut ws).await;
    assert_eq!(first["type"], "hello");
    assert_eq!(first["model"], "mock-model");
    assert_eq!(first["mode"], "ask");
    assert_eq!(first["ctx_window"], 128_000);

    // 2) 上行:prompt 到达宿主动作面
    ws.send(Message::Text(
        json!({"type": "prompt", "text": "你好"}).to_string(),
    ))
    .await
    .unwrap();
    let got = tokio::time::timeout(Duration::from_secs(2), h.ui.recv())
        .await
        .expect("宿主应收到 prompt")
        .expect("通道未关闭");
    assert!(matches!(got, UiInput::Prompt(t) if t == "你好"));

    // 3) 下行:宿主事件投影成帧
    let frame = bridge::frame(&znaide_host::HostEvent::Session(
        znaide_core::session::SessionEvent::TextDelta("流式".into()),
    ))
    .unwrap();
    h.bus.send(frame).unwrap();
    let ev = next_json(&mut ws).await;
    assert_eq!(ev["type"], "event");
    assert_eq!(ev["kind"], "text_delta");
    assert_eq!(ev["text"], "流式");
}

#[tokio::test]
async fn ws_forwards_command_and_queue_controls() {
    let mut h = start().await;
    let (mut ws, _) =
        tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{}/ws?token={TOKEN}", h.port))
            .await
            .unwrap();
    let _ = next_json(&mut ws).await;

    for (payload, expect) in [
        (json!({"type": "command", "line": "/help"}), "cmd"),
        (json!({"type": "unqueue", "text": "排队那条"}), "unqueue"),
        (json!({"type": "clear"}), "clear"),
        (json!({"type": "mode", "mode": "acceptEdits"}), "mode"),
        (json!({"type": "persona", "name": "严谨"}), "persona"),
        (json!({"type": "provider", "name": "deepseek"}), "provider"),
        (json!({"type": "compact"}), "compact"),
        (json!({"type": "undo_list"}), "undo_list"),
        (json!({"type": "undo", "index": 2}), "undo"),
        (json!({"type": "resume", "id": "s1"}), "resume"),
        (json!({"type": "delete_session", "id": "s1"}), "delete"),
        (
            json!({"type": "rename_session", "id": "s1", "note": "日报"}),
            "rename",
        ),
    ] {
        ws.send(Message::Text(payload.to_string())).await.unwrap();
        let got = tokio::time::timeout(Duration::from_secs(2), h.ui.recv())
            .await
            .expect("宿主应收到动作")
            .expect("通道未关闭");
        match (expect, got) {
            ("cmd", UiInput::Command(line)) => assert_eq!(line, "/help"),
            ("unqueue", UiInput::Unqueue(text)) => assert_eq!(text, "排队那条"),
            ("clear", UiInput::Clear) => {}
            ("mode", UiInput::SetMode(m)) => assert_eq!(m, "acceptEdits"),
            ("persona", UiInput::SetPersona(n)) => assert_eq!(n, "严谨"),
            ("provider", UiInput::SetProvider(n)) => assert_eq!(n, "deepseek"),
            ("compact", UiInput::Compact) => {}
            ("undo_list", UiInput::UndoList) => {}
            ("undo", UiInput::Undo(i)) => assert_eq!(i, 2),
            ("resume", UiInput::ResumeSession(id)) => assert_eq!(id, "s1"),
            ("delete", UiInput::DeleteSession(id)) => assert_eq!(id, "s1"),
            ("rename", UiInput::RenameSession { id, note }) => {
                assert_eq!(id, "s1");
                assert_eq!(note, "日报");
            }
            (_, other) => panic!("动作映射不对:{other:?}"),
        }
    }
}

async fn next_json(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Value {
    let msg = tokio::time::timeout(Duration::from_secs(2), ws.next())
        .await
        .expect("等帧超时")
        .expect("连接关闭")
        .expect("读帧失败");
    let text = msg.into_text().expect("文本帧");
    serde_json::from_str(&text).expect("JSON 帧")
}

#[tokio::test]
async fn evil_origin_is_rejected() {
    let h = start().await;
    let ws_url = format!("ws://127.0.0.1:{}/ws?token={TOKEN}", h.port);
    let mut req = ws_url.into_client_request().unwrap();
    req.headers_mut()
        .insert("origin", "http://evil.example".parse().unwrap());
    let err = tokio_tungstenite::connect_async(req).await.unwrap_err();
    match err {
        tokio_tungstenite::tungstenite::Error::Http(resp) => {
            assert_eq!(resp.status(), 403, "跨站 Origin 必须被拒");
        }
        other => panic!("期望 HTTP 403,实际:{other}"),
    }
}

#[tokio::test]
async fn pending_ask_survives_reload() {
    let h = start().await;
    let ask = bridge::frame(&znaide_host::HostEvent::Ask(
        znaide_host::ask::Ask::Permission(znaide_host::ask::PermissionAsk {
            title: "执行命令".into(),
            body: "rm -rf /tmp/x".into(),
            warning: Some("高危".into()),
            allow_always: false,
        }),
    ))
    .unwrap();
    h.live.lock().unwrap().pending_ask = Some(ask);
    h.live.lock().unwrap().session_id = Some("abc123".into());

    let (mut ws, _) =
        tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{}/ws?token={TOKEN}", h.port))
            .await
            .unwrap();
    let hello = next_json(&mut ws).await;
    assert_eq!(hello["session_id"], "abc123");
    assert_eq!(hello["pending_ask"]["kind"], "permission");
    assert_eq!(hello["pending_ask"]["allow_always"], false);
}

/// D2:最后一个页面断开时,未决询问按拒绝处理(失败关闭),不让回合悬着。
#[tokio::test]
async fn closing_the_last_page_denies_pending_ask() {
    let mut h = start().await;
    let ask = bridge::frame(&znaide_host::HostEvent::Ask(znaide_host::ask::Ask::Rounds(
        znaide_host::ask::RoundsAsk { used: 200 },
    )))
    .unwrap();
    h.live.lock().unwrap().pending_ask = Some(ask);

    let (mut ws, _) =
        tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{}/ws?token={TOKEN}", h.port))
            .await
            .unwrap();
    // 读到 hello 说明服务端已经接管这条连接(计数已 +1)
    let _ = next_json(&mut ws).await;
    drop(ws);

    let got = tokio::time::timeout(Duration::from_secs(2), h.ui.recv())
        .await
        .expect("断开后应自动回答")
        .expect("通道未关闭");
    assert!(
        matches!(got, UiInput::Rounds { cont: false }),
        "失败关闭应按“收尾”回答,实际:{got:?}"
    );
    assert!(h.live.lock().unwrap().pending_ask.is_none());
}
