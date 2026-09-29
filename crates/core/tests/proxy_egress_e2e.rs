//! 代理"统一出口"的端到端验证:模型请求 / 网页抓取 / 版本更新三处必须走同一个
//! `EffectiveProxy` 快照。
//!
//! 办法是起一个**记账**用的假 HTTP 服务:它把收到的请求行记下来再回一个合法响应。
//! 请求走了代理,它的第一行就是绝对 URI(`POST http://主机/路径 HTTP/1.1`);
//! 没走代理,它一条都收不到 —— 于是"到底走了哪条出口"是可以断言的,而不是靠读代码。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use znaide_core::config::{EffectiveProxy, Resolved};

/// 起一个记账服务:读完请求头记下第一行,回一个合法的 chat 响应(任何请求都回)
async fn recorder() -> (String, Arc<Mutex<Vec<String>>>, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let lines: Arc<Mutex<Vec<String>>> = Arc::default();
    let hits = Arc::new(AtomicUsize::new(0));
    let (l, h) = (lines.clone(), hits.clone());
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let (l, h) = (l.clone(), h.clone());
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = match sock.read(&mut buf).await {
                    Ok(n) => n,
                    Err(_) => return,
                };
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                l.lock()
                    .unwrap()
                    .push(head.lines().next().unwrap_or("").to_string());
                h.fetch_add(1, Ordering::SeqCst);
                let body = r#"{"choices":[{"message":{"role":"assistant","content":"pong"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            });
        }
    });
    (format!("http://{addr}"), lines, hits)
}

fn resolved(base_url: &str, proxy: EffectiveProxy) -> Resolved {
    Resolved {
        model: "m".into(),
        base_url: base_url.to_string(),
        api_key: None,
        provider_name: "mock".into(),
        context_window: None,
        proxy,
    }
}

/// 这些用例会改代理环境变量,自己串行一把(同一二进制内并行跑)
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn clear_proxy_env() {
    for v in [
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "NO_PROXY",
        "no_proxy",
    ] {
        std::env::remove_var(v);
    }
}

/// manual 档:模型请求经过代理,且代理看到的是指向端点的绝对 URI(没直连)
#[tokio::test]
async fn manual_proxy_carries_the_model_request() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_proxy_env();

    let (origin, _, origin_hits) = recorder().await;
    let (proxy, proxy_lines, proxy_hits) = recorder().await;
    let cfg = resolved(&format!("{origin}/v1"), EffectiveProxy::Manual(proxy.clone()));

    let client = znaide_core::llm::OpenAiClient::new(&cfg).unwrap();
    let reply = client
        .chat(&[znaide_core::llm::ChatMessage::user("hi")], None)
        .await
        .unwrap();
    assert_eq!(reply.content.as_deref(), Some("pong"));

    assert_eq!(origin_hits.load(Ordering::SeqCst), 0, "不该直连端点");
    assert_eq!(proxy_hits.load(Ordering::SeqCst), 1, "应当经过代理");
    let first = proxy_lines.lock().unwrap()[0].clone();
    assert!(first.starts_with("POST http://"), "代理收到的应是绝对 URI:{first}");
    assert!(first.contains(&origin), "代理请求要指向端点:{first}");
}

/// direct 档:环境变量里挂着代理也一律直连(排查用,必须绝对)
#[tokio::test]
async fn direct_ignores_the_env_proxy() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_proxy_env();

    let (origin, _, origin_hits) = recorder().await;
    let (proxy, _, proxy_hits) = recorder().await;
    std::env::set_var("ALL_PROXY", &proxy);
    std::env::set_var("HTTP_PROXY", &proxy);
    std::env::set_var("HTTPS_PROXY", &proxy);

    let cfg = resolved(&format!("{origin}/v1"), EffectiveProxy::Direct);
    let client = znaide_core::llm::OpenAiClient::new(&cfg).unwrap();
    client
        .chat(&[znaide_core::llm::ChatMessage::user("hi")], None)
        .await
        .unwrap();

    assert_eq!(origin_hits.load(Ordering::SeqCst), 1, "直连才该命中端点");
    assert_eq!(proxy_hits.load(Ordering::SeqCst), 0, "direct 档必须无视环境变量");

    clear_proxy_env();
}

/// auto 档 = 旧行为:交给 reqwest 读标准环境变量
#[tokio::test]
async fn auto_follows_the_env_proxy() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_proxy_env();

    let (origin, _, origin_hits) = recorder().await;
    let (proxy, _, proxy_hits) = recorder().await;
    std::env::set_var("ALL_PROXY", &proxy);
    std::env::set_var("HTTP_PROXY", &proxy);

    let cfg = resolved(&format!("{origin}/v1"), EffectiveProxy::Auto);
    let client = znaide_core::llm::OpenAiClient::new(&cfg).unwrap();
    client
        .chat(&[znaide_core::llm::ChatMessage::user("hi")], None)
        .await
        .unwrap();

    assert_eq!(proxy_hits.load(Ordering::SeqCst), 1, "auto 档要跟随环境变量");
    assert_eq!(origin_hits.load(Ordering::SeqCst), 0);

    clear_proxy_env();
}

/// web_fetch 与模型请求同一个出口(ToolContext 里的那份快照)
#[tokio::test]
async fn web_fetch_uses_the_same_egress() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_proxy_env();

    let (proxy, lines, _) = recorder().await;
    let eff = EffectiveProxy::Manual(proxy.clone());
    let perm =
        znaide_core::permissions::Permission::new(znaide_core::permissions::Mode::BypassPermissions);
    let ctx = znaide_core::tools::ToolContext {
        cwd: std::path::Path::new("/tmp"),
        permission: &perm,
        session_id: "proxy-e2e",
        cancel: None,
        events: None,
        proxy: &eff,
    };

    let out = znaide_core::tools::execute(
        "web_fetch",
        serde_json::json!({ "url": "http://example.invalid/x" }),
        &ctx,
    )
    .await;
    assert!(out.is_ok(), "抓取应成功(由代理回包):{out:?}");

    let first = lines.lock().unwrap().first().cloned().unwrap_or_default();
    assert!(
        first.starts_with("GET http://example.invalid/x"),
        "网页抓取也要走代理:{first}"
    );
}

/// 更新下载的客户端同样认这份快照:指向一个必定连不上的地址,
/// 还能拿到响应就只可能是走了代理
#[tokio::test]
async fn update_client_honours_the_proxy() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_proxy_env();

    let (proxy, _, hits) = recorder().await;
    let client =
        znaide_core::update::http_client(&EffectiveProxy::Manual(proxy.clone())).unwrap();
    let r = client.get("http://127.0.0.1:1/dead").send().await;
    assert!(
        r.is_ok(),
        "更新客户端也该走代理:{:?}",
        r.err().map(|e| e.to_string())
    );
    assert!(hits.load(Ordering::SeqCst) >= 1);
}
