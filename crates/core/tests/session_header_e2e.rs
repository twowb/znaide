//! `x-opencode-session` 稳定会话头 mock 集成测试(07 §2):
//! 起本地 TCP mock(仿 mock_e2e.rs 画风)，把收到的头记入 `seen`，断言双协议
//! 非流式/流式/validate/`GET /models` 的头行为，以及 Session 供给语义
//! (新建/load_history/reconfigure)。

use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::{timeout, Duration};
use tokio_util::sync::CancellationToken;
use znaide_core::config::{ProtocolKind, Resolved};
use znaide_core::llm::openai::StreamEvent;
use znaide_core::llm::{build_llm_client, ChatMessage, Usage};
use znaide_core::permissions::Mode;
use znaide_core::session::Session;

/// 收到的请求头记录:(path, x-opencode-session 值)
type Seen = Arc<Mutex<Vec<(String, Option<String>)>>>;

/// 启动 mock:读完整请求，把 head 原文 + body 交给 handler。
async fn spawn_header_mock<F>(handler: F) -> String
where
    F: FnMut(String, serde_json::Value) -> Vec<u8> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handler = Arc::new(tokio::sync::Mutex::new(handler));
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let handler = handler.clone();
            tokio::spawn(async move {
                let _ = timeout(Duration::from_secs(10), async {
                    let mut head = Vec::new();
                    let mut tmp = [0u8; 1024];
                    let mut header_end: Option<usize> = None;
                    while header_end.is_none() {
                        let n = sock.read(&mut tmp).await?;
                        if n == 0 {
                            return Ok::<(), std::io::Error>(());
                        }
                        head.extend_from_slice(&tmp[..n]);
                        header_end = head.windows(4).position(|w| w == b"\r\n\r\n");
                    }
                    let he = header_end.unwrap();
                    let head_str = String::from_utf8_lossy(&head[..he]).to_string();
                    let content_length: usize = head_str
                        .lines()
                        .find_map(|l| {
                            let l = l.trim();
                            l.strip_prefix("Content-Length:")
                                .or_else(|| l.strip_prefix("content-length:"))
                                .map(|v| v.trim().parse().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    while head.len() < he + 4 + content_length {
                        let n = sock.read(&mut tmp).await?;
                        if n == 0 {
                            break;
                        }
                        head.extend_from_slice(&tmp[..n]);
                    }
                    let body: serde_json::Value = if content_length > 0 {
                        serde_json::from_slice(&head[he + 4..he + 4 + content_length])
                            .unwrap_or(json!({}))
                    } else {
                        json!({})
                    };
                    let mut guard = handler.lock().await;
                    let resp = guard(head_str, body);
                    drop(guard);
                    sock.write_all(&resp).await?;
                    let _ = sock.shutdown().await;
                    Ok::<(), std::io::Error>(())
                })
                .await;
            });
        }
    });
    format!("http://{addr}/v1")
}

/// 从请求 head 取请求 path(首行 `POST /v1/chat/completions HTTP/1.1`)。
fn request_path(head: &str) -> String {
    head.lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("")
        .to_string()
}

/// 从请求 head 取指定头(名大小写不敏感 ،值原样)。
fn request_header(head: &str, name: &str) -> Option<String> {
    let want = name.to_lowercase();
    head.lines().skip(1).find_map(|l| {
        let (k, v) = l.split_once(':')?;
        if k.trim().to_lowercase() == want {
            Some(v.trim().to_string())
        } else {
            None
        }
    })
}

fn http_json_resp(body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .into_bytes()
}

fn sse_resp(events: &[&str]) -> Vec<u8> {
    let mut payload = String::new();
    for e in events {
        payload.push_str("data: ");
        payload.push_str(e);
        payload.push_str("\n\n");
    }
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{payload}"
    )
    .into_bytes()
}

const CHAT_JSON: &str = r#"{"choices":[{"message":{"role":"assistant","content":"hi"}}]}"#;
const RESP_JSON: &str = r#"{"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"hi"}]}],"usage":{"input_tokens":1,"output_tokens":1}}"#;

/// 通用 mock:记录头，按 path/body 返回对应协议的非流式或流式响应。
fn recording_handler(seen: Seen) -> impl FnMut(String, serde_json::Value) -> Vec<u8> {
    move |head, body| {
        let path = request_path(&head);
        let hv = request_header(&head, "x-opencode-session");
        seen.lock().unwrap().push((path.clone(), hv));
        if path.ends_with("/models") {
            return http_json_resp(r#"{"data":[{"id":"m"}]}"#);
        }
        let stream = body
            .get("stream")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let is_responses = body.get("input").is_some();
        if stream {
            if is_responses {
                sse_resp(&[
                    r#"{"type":"response.output_text.delta","output_index":0,"delta":"hi"}"#,
                    r#"{"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}}"#,
                ])
            } else {
                sse_resp(&[
                    r#"{"choices":[{"delta":{"content":"hi"},"finish_reason":null}]}"#,
                    r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
                    "[DONE]",
                ])
            }
        } else if is_responses {
            http_json_resp(RESP_JSON)
        } else {
            http_json_resp(CHAT_JSON)
        }
    }
}

fn test_cfg(base: String, protocol: ProtocolKind, enabled: bool) -> Resolved {
    Resolved {
        model: "m".into(),
        base_url: base,
        api_key: None,
        provider_name: "mock".into(),
        context_window: None,
        protocol,
        session_header_enabled: enabled,
        retry: znaide_core::config::RetryConfig::disabled(),
        proxy: znaide_core::config::EffectiveProxy::Direct,
    }
}

fn test_cwd(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("znaide_sh_{tag}_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn new_session(
    cfg: &Resolved,
    cwd: &std::path::Path,
    session_id: Option<String>,
    enabled: bool,
) -> Session {
    let llm = build_llm_client(cfg).unwrap();
    Session::new(
        llm,
        cfg.protocol,
        cwd.to_path_buf(),
        Mode::BypassPermissions,
        None,
        CancellationToken::new(),
        false,
        session_id,
        None,
        "",
        enabled,
    )
    .unwrap()
}

/// 开 + chat 非流式(Session 链路):同一会话内头值 = session_id。
#[tokio::test]
async fn chat_nonstream_carries_stable_id() {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let base = spawn_header_mock(recording_handler(seen.clone())).await;
    let cwd = test_cwd("chat");
    let cfg = test_cfg(base, ProtocolKind::Chat, true);
    let mut session = new_session(&cfg, &cwd, Some("e2e-sess-1".into()), true);
    let result = session.run_turn("hi").await.unwrap();
    assert_eq!(result.text, "hi");
    let seen = seen.lock().unwrap();
    assert!(!seen.is_empty(), "应至少发出一次模型请求");
    for (path, hv) in seen.iter() {
        assert_eq!(
            hv.as_deref(),
            Some("e2e-sess-1"),
            "path={path} 头值应为会话 ID"
        );
    }
    std::fs::remove_dir_all(cwd).ok();
}

/// 开 + chat 流式(client 级):SSE 路径同样带头。
#[tokio::test]
async fn chat_stream_carries_stable_id() {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let base = spawn_header_mock(recording_handler(seen.clone())).await;
    let cfg = test_cfg(base, ProtocolKind::Chat, true);
    let mut client = build_llm_client(&cfg).unwrap();
    client.set_session_header(true, "e2e-stream-1");
    let mut deltas = Vec::new();
    let reply = client
        .chat_stream(&[ChatMessage::user("hi")], None, &mut |ev| match ev {
            StreamEvent::TextDelta(t) => deltas.push(t),
            StreamEvent::ReasoningDelta(_) => {}
        })
        .await
        .unwrap();
    assert_eq!(reply.content.as_deref(), Some("hi"));
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].1.as_deref(), Some("e2e-stream-1"));
}

/// 开 + responses 非流式(Session 链路)/流式(client 级)。
#[tokio::test]
async fn responses_nonstream_and_stream_carry_id() {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let base = spawn_header_mock(recording_handler(seen.clone())).await;
    let cwd = test_cwd("resp");
    let cfg = test_cfg(base, ProtocolKind::Response, true);
    // 非流式经 Session
    let mut session = new_session(&cfg, &cwd, Some("e2e-resp-1".into()), true);
    let result = session.run_turn("hi").await.unwrap();
    assert_eq!(result.text, "hi");
    // 流式经 client
    let mut client = build_llm_client(&cfg).unwrap();
    client.set_session_header(true, "e2e-resp-1");
    let mut deltas = Vec::new();
    let reply = client
        .chat_stream(&[ChatMessage::user("hi")], None, &mut |ev| match ev {
            StreamEvent::TextDelta(t) => deltas.push(t),
            StreamEvent::ReasoningDelta(_) => {}
        })
        .await
        .unwrap();
    assert_eq!(reply.content.as_deref(), Some("hi"));
    assert_eq!(deltas.join(""), "hi");
    let seen = seen.lock().unwrap();
    assert!(seen.len() >= 2, "非流式 + 流式至少两次请求");
    for (path, hv) in seen.iter() {
        assert_eq!(hv.as_deref(), Some("e2e-resp-1"), "path={path}");
    }
    std::fs::remove_dir_all(cwd).ok();
}

/// 关 + 全协议:服务端收不到该头。
#[tokio::test]
async fn disabled_sends_no_header() {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let base = spawn_header_mock(recording_handler(seen.clone())).await;
    let cwd = test_cwd("off");
    for protocol in [ProtocolKind::Chat, ProtocolKind::Response] {
        let cfg = test_cfg(base.clone(), protocol, false);
        let mut session = new_session(&cfg, &cwd, Some("e2e-off".into()), false);
        session.run_turn("hi").await.unwrap();
    }
    let seen = seen.lock().unwrap();
    assert!(!seen.is_empty());
    for (path, hv) in seen.iter() {
        assert!(hv.is_none(), "path={path} 开关关时不得发头");
    }
    std::fs::remove_dir_all(cwd).ok();
}

/// load_history 恢复后:头的值切换为老会话 ID。
#[tokio::test]
async fn load_history_switches_header() {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let base = spawn_header_mock(recording_handler(seen.clone())).await;
    let cwd = test_cwd("resume");
    // 老会话文件(stem 即老 ID)
    let hp = cwd.join("e2e-old-9.jsonl");
    std::fs::write(
        &hp,
        "{\"meta\":{\"headless\":false,\"schema\":\"c4a23d6e50c7\"}}\n{\"role\":\"user\",\"content\":\"hi\"}\n",
    )
    .unwrap();
    let cfg = test_cfg(base, ProtocolKind::Chat, true);
    let mut session = new_session(&cfg, &cwd, Some("e2e-new".into()), true);
    session.load_history(&hp).unwrap();
    assert_eq!(session.session_id(), "e2e-old-9");
    session.run_turn("hi").await.unwrap();
    let seen = seen.lock().unwrap();
    assert!(!seen.is_empty());
    for (path, hv) in seen.iter() {
        assert_eq!(
            hv.as_deref(),
            Some("e2e-old-9"),
            "path={path} 恢复后头应为老 ID"
        );
    }
    std::fs::remove_dir_all(cwd).ok();
}

/// reconfigure 开→关:同一 Session 后续请求头消失。
#[tokio::test]
async fn reconfigure_off_drops_header() {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let base = spawn_header_mock(recording_handler(seen.clone())).await;
    let cwd = test_cwd("recfg");
    let cfg = test_cfg(base, ProtocolKind::Chat, true);
    let mut session = new_session(&cfg, &cwd, Some("e2e-recfg".into()), true);
    session.run_turn("hi").await.unwrap();
    // 同协议关开关(连接池不断言，只断言头)
    let mut off = cfg.clone();
    off.session_header_enabled = false;
    session.reconfigure(&off);
    session.run_turn("hi").await.unwrap();
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].1.as_deref(), Some("e2e-recfg"), "重配前应带头");
    assert!(seen[1].1.is_none(), "重配关后头应消失");
    std::fs::remove_dir_all(cwd).ok();
}

/// validate 与 GET /models:开关开时同样带头。
#[tokio::test]
async fn validate_and_list_models_carry_header() {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let base = spawn_header_mock(recording_handler(seen.clone())).await;
    let cfg = test_cfg(base, ProtocolKind::Chat, true);
    let mut client = build_llm_client(&cfg).unwrap();
    client.set_session_header(true, "e2e-probe-1");
    client.validate().await.unwrap();
    client.list_models().await.unwrap();
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    for (path, hv) in seen.iter() {
        assert_eq!(hv.as_deref(), Some("e2e-probe-1"), "path={path}");
    }
}

/// 头值在请求头表里的确切名字(防大小写/拼写漂移)。
#[tokio::test]
async fn header_name_is_exact() {
    let seen_raw: Arc<Mutex<Vec<HashMap<String, String>>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_clone = seen_raw.clone();
    let base = spawn_header_mock(move |head, _body| {
        let mut m = HashMap::new();
        for l in head.lines().skip(1) {
            if let Some((k, v)) = l.split_once(':') {
                m.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
        seen_clone.lock().unwrap().push(m);
        http_json_resp(CHAT_JSON)
    })
    .await;
    let cfg = test_cfg(base, ProtocolKind::Chat, true);
    let mut client = build_llm_client(&cfg).unwrap();
    client.set_session_header(true, "e2e-name");
    client.chat(&[ChatMessage::user("hi")], None).await.unwrap();
    let seen = seen_raw.lock().unwrap();
    assert_eq!(seen.len(), 1);
    // reqwest 把头名常规化为小写；文档与代码统一小写字面量
    assert_eq!(
        seen[0].get("x-opencode-session").map(|s| s.as_str()),
        Some("e2e-name")
    );
    assert!(!seen[0].contains_key("X-Opencode-Session"));
    let _ = Usage::default();
}
