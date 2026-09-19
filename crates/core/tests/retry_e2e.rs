//! 弱网重试端到端：空回复 / 500 / 400 在真实 Session + mock HTTP 下的行为。
//! mock 服务器与 mock_e2e.rs 同构（按 Content-Length 精确读请求，响应后关连接）。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::{timeout, Duration};
use tokio_util::sync::CancellationToken;
use znaide_core::config::{EffectiveProxy, Resolved, RetryConfig};
use znaide_core::llm::build_llm_client;
use znaide_core::permissions::Mode;
use znaide_core::session::Session;

/// 启动 mock：每个连接读完整请求，调用 handler 生成响应
async fn spawn_mock<F>(handler: F) -> String
where
    F: FnMut(u32, serde_json::Value) -> Vec<u8> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let counter = Arc::new(AtomicUsize::new(0));
    let handler = Arc::new(tokio::sync::Mutex::new(handler));
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let counter = counter.clone();
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
                    let head_str = String::from_utf8_lossy(&head[..he]);
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
                            .unwrap_or(serde_json::json!({}))
                    } else {
                        serde_json::json!({})
                    };
                    let n = counter.load(Ordering::SeqCst);
                    counter.store(n + 1, Ordering::SeqCst);
                    let mut guard = handler.lock().await;
                    let resp = guard(n as u32, body);
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

fn http_json_resp(body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .into_bytes()
}

fn http_status_resp(status: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .into_bytes()
}

const EMPTY_REPLY: &str = r#"{"id":"1","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":""},"finish_reason":"stop"}]}"#;
const OK_REPLY: &str = r#"{"id":"2","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"重试后成功"},"finish_reason":"stop"}]}"#;

fn test_cfg(base: String) -> Resolved {
    Resolved {
        model: "m".into(),
        base_url: base,
        api_key: None,
        provider_name: "mock".into(),
        context_window: None,
        protocol: znaide_core::config::ProtocolKind::Chat,
        session_header_enabled: false,
        retry: RetryConfig::disabled(),
        proxy: EffectiveProxy::Direct,
    }
}

fn test_session(cfg: &Resolved, cwd: std::path::PathBuf) -> Session {
    let llm = build_llm_client(cfg).unwrap();
    Session::new(
        llm,
        cfg.protocol,
        cwd,
        Mode::BypassPermissions,
        None, // 无头 = 非流式 chat
        CancellationToken::new(),
        false,
        Some(format!("retry-e2e-{}", std::process::id())),
        None,
        "",
        false,
    )
    .unwrap()
}

fn test_cwd(tag: &str) -> std::path::PathBuf {
    let cwd = std::env::temp_dir().join(format!("znaide_retry_{tag}_{}", std::process::id()));
    std::fs::create_dir_all(&cwd).unwrap();
    cwd
}

/// 空回复 → 重试一次后成功：最终文本正常，只落一条 assistant 历史，调了 2 次
#[tokio::test]
async fn retry_recovers_from_empty_reply() {
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_clone = hits.clone();
    let base = spawn_mock(move |_n, _body| {
        let n = hits_clone.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            http_json_resp(EMPTY_REPLY)
        } else {
            http_json_resp(OK_REPLY)
        }
    })
    .await;

    let cwd = test_cwd("empty_ok");
    let cfg = test_cfg(base);
    let mut session = test_session(&cfg, cwd.clone());
    session.set_retry(RetryConfig::enabled_with(3));
    let result = session.run_turn("hi").await.unwrap();
    assert_eq!(result.text, "重试后成功");
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    std::fs::remove_dir_all(cwd).ok();
}

/// 空回复耗尽：追加 2 次共调 3 次，文本为占位且不报错
#[tokio::test]
async fn retry_exhausts_empty_reply() {
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_clone = hits.clone();
    let base = spawn_mock(move |_n, _body| {
        hits_clone.fetch_add(1, Ordering::SeqCst);
        http_json_resp(EMPTY_REPLY)
    })
    .await;

    let cwd = test_cwd("empty_out");
    let cfg = test_cfg(base);
    let mut session = test_session(&cfg, cwd.clone());
    session.set_retry(RetryConfig::enabled_with(2));
    let result = session.run_turn("hi").await.unwrap();
    assert_eq!(result.text, "(模型无文本输出)");
    assert_eq!(hits.load(Ordering::SeqCst), 3);
    std::fs::remove_dir_all(cwd).ok();
}

/// 500 → 重试后成功
#[tokio::test]
async fn retry_recovers_from_500() {
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_clone = hits.clone();
    let base = spawn_mock(move |_n, _body| {
        let n = hits_clone.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            http_status_resp("500 Internal Server Error", r#"{"error":"boom"}"#)
        } else {
            http_json_resp(OK_REPLY)
        }
    })
    .await;

    let cwd = test_cwd("err500_ok");
    let cfg = test_cfg(base);
    let mut session = test_session(&cfg, cwd.clone());
    session.set_retry(RetryConfig::enabled_with(3));
    let result = session.run_turn("hi").await.unwrap();
    assert_eq!(result.text, "重试后成功");
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    std::fs::remove_dir_all(cwd).ok();
}

/// 默认关闭：空回复不重试，直接占位收尾且只调 1 次
#[tokio::test]
async fn disabled_retry_keeps_old_behavior() {
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_clone = hits.clone();
    let base = spawn_mock(move |_n, _body| {
        hits_clone.fetch_add(1, Ordering::SeqCst);
        http_json_resp(EMPTY_REPLY)
    })
    .await;

    let cwd = test_cwd("disabled");
    let cfg = test_cfg(base);
    let mut session = test_session(&cfg, cwd.clone());
    // 不开重试（默认）
    let result = session.run_turn("hi").await.unwrap();
    assert_eq!(result.text, "(模型无文本输出)");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    std::fs::remove_dir_all(cwd).ok();
}

/// 400 不重试：直接 Err，只调 1 次
#[tokio::test]
async fn non_retryable_400_does_not_retry() {
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_clone = hits.clone();
    let base = spawn_mock(move |_n, _body| {
        hits_clone.fetch_add(1, Ordering::SeqCst);
        http_status_resp("400 Bad Request", r#"{"error":"bad request"}"#)
    })
    .await;

    let cwd = test_cwd("bad400");
    let cfg = test_cfg(base);
    let mut session = test_session(&cfg, cwd.clone());
    session.set_retry(RetryConfig::enabled_with(3));
    let err = match session.run_turn("hi").await {
        Ok(_) => panic!("400 应返回 Err"),
        Err(e) => e,
    };
    assert!(format!("{err:#}").contains("400"), "unexpected: {err:#}");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    std::fs::remove_dir_all(cwd).ok();
}
