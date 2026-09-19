//! need03 代理端到端（07 §3）：黑洞代理 + 本地 mock，验证各档真实行为。
//! mock 风格仿 quick_cfg_e2e.rs（按 Content-Length 读完请求再回响应）。
//! 黑洞地址用 127.0.0.1:9（discard 端口，必然拒连；connect 超时 10s 内必失败）。

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use znaide_core::config::{
    Config, EffectiveProxy, ProtocolKind, ProxyConfig, ProxyMode, Resolved, RetryConfig,
};

static E2E_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 黑洞代理地址（必然拒连）
const BLACKHOLE: &str = "http://127.0.0.1:9";

fn clear_proxy_env() {
    for v in [
        "ZNAIDE_PROXY",
        "ZNAIDE_NO_PROXY",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "NO_PROXY",
        "no_proxy",
        "ZNAIDE_RETRY",
        "ZNAIDE_NO_RETRY",
    ] {
        std::env::remove_var(v);
    }
}

/// 加锁 + 干净代理 env + 独立数据目录。返回 guard（await 前 drop）与目录（收尾删）。
fn lock_env(name: &str) -> (std::sync::MutexGuard<'static, ()>, std::path::PathBuf) {
    let g = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_proxy_env();
    let dir = std::env::temp_dir().join(format!("znaide_px_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("ZNAIDE_DATA_DIR", &dir);
    (g, dir)
}

fn unlock_env(dir: &std::path::Path) {
    std::env::remove_var("ZNAIDE_DATA_DIR");
    clear_proxy_env();
    let _ = std::fs::remove_dir_all(dir);
}

/// 固定响应的 mock（读完请求→回固定 HTTP 包→关连接）
async fn spawn_fixed(status: &'static str, body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut tmp = [0u8; 4096];
                let mut header_end: Option<usize> = None;
                while header_end.is_none() {
                    let Ok(n) = sock.read(&mut tmp).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    head.extend_from_slice(&tmp[..n]);
                    header_end = head.windows(4).position(|w| w == b"\r\n\r\n");
                    if head.len() > 65536 {
                        return;
                    }
                }
                let he = header_end.unwrap();
                let head_str = String::from_utf8_lossy(&head[..he]);
                let content_length: usize = head_str
                    .lines()
                    .find_map(|l| {
                        l.trim()
                            .strip_prefix("Content-Length:")
                            .or_else(|| l.trim().strip_prefix("content-length:"))
                            .map(|v| v.trim().parse().unwrap_or(0))
                    })
                    .unwrap_or(0);
                let mut read = head.len() - (he + 4);
                while read < content_length {
                    let Ok(n) = sock.read(&mut tmp).await else {
                        break;
                    };
                    if n == 0 {
                        break;
                    }
                    read += n;
                }
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    format!("http://{addr}/v1")
}

const CHAT_OK: &str = r#"{"id":"x","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}]}"#;

fn chat_cfg(base: &str, proxy: EffectiveProxy) -> Resolved {
    Resolved {
        model: "m".into(),
        base_url: base.into(),
        api_key: None,
        provider_name: "mock".into(),
        context_window: None,
        protocol: ProtocolKind::Chat,
        session_header_enabled: false,
        retry: RetryConfig::disabled(),
        proxy,
    }
}

const RESP_OK: &str = r#"{"id":"resp_1","status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"hi"}]}],"usage":{"input_tokens":3,"output_tokens":2}}"#;

fn responses_cfg(base: &str, proxy: EffectiveProxy) -> Resolved {
    Resolved {
        protocol: ProtocolKind::Response,
        ..chat_cfg(base, proxy)
    }
}

/// E1 直连：Direct + 环境故意设黑洞 HTTPS_PROXY → mock 仍通
/// （证明 Off/Direct 真忽略环境）
#[tokio::test]
async fn e1_direct_ignores_blackhole_env() {
    let base = spawn_fixed("200 OK", CHAT_OK).await;
    let (g, dir) = lock_env("e1");
    std::env::set_var("HTTPS_PROXY", BLACKHOLE);
    let cfg = chat_cfg(&base, EffectiveProxy::Direct);
    drop(g);
    znaide_core::llm::openai::probe_chat(&cfg).await.unwrap();
    unlock_env(&dir);
}

/// E2 跟随环境：Auto + 黑洞 → probe 失败（env 被真读取）；显式 Direct → 通
#[tokio::test]
async fn e2_auto_follows_env() {
    let base = spawn_fixed("200 OK", CHAT_OK).await;
    let (g, dir) = lock_env("e2");
    std::env::set_var("HTTPS_PROXY", BLACKHOLE);
    // resolve 出 Via(黑洞)
    let cfg = Config::default();
    assert_eq!(
        cfg.resolve_proxy(None, false),
        EffectiveProxy::Via(BLACKHOLE.into())
    );
    let via = chat_cfg(&base, EffectiveProxy::Via(BLACKHOLE.into()));
    let direct = chat_cfg(&base, EffectiveProxy::Direct);
    drop(g);
    assert!(
        znaide_core::llm::openai::probe_chat(&via).await.is_err(),
        "Auto 跟随黑洞代理应失败"
    );
    znaide_core::llm::openai::probe_chat(&direct).await.unwrap();
    unlock_env(&dir);
}

/// E3 手动：Manual(黑洞)落盘 → resolve 出 Via → probe 失败
#[tokio::test]
async fn e3_manual_blackhole_fails() {
    let base = spawn_fixed("200 OK", CHAT_OK).await;
    let (g, dir) = lock_env("e3");
    let mut cfg = Config::default();
    cfg.save_proxy(ProxyConfig {
        mode: ProxyMode::Manual,
        url: Some(BLACKHOLE.into()),
    })
    .unwrap();
    let back = Config::load().unwrap();
    let resolved = back.resolve(None, None, None, None, None, None).unwrap();
    assert_eq!(resolved.proxy, EffectiveProxy::Via(BLACKHOLE.into()));
    let probe = chat_cfg(&base, resolved.proxy.clone());
    drop(g);
    assert!(
        znaide_core::llm::openai::probe_chat(&probe).await.is_err(),
        "Manual 黑洞应失败"
    );
    unlock_env(&dir);
}

/// E4 重建：reconfigure(Direct → Manual(黑洞) → Direct) 三连切，
/// Direct 档通、Manual 档失败、切回通（证明重建生效且可逆）
#[tokio::test]
async fn e4_reconfigure_rebuilds_and_reverts() {
    let base = spawn_fixed("200 OK", CHAT_OK).await;
    let (_g, dir) = lock_env("e4");
    let mut client =
        znaide_core::llm::openai::OpenAiClient::new(&chat_cfg(&base, EffectiveProxy::Direct))
            .unwrap();
    client.validate().await.unwrap();
    // 切 Manual(黑洞)：重建后失败
    client.reconfigure(&chat_cfg(&base, EffectiveProxy::Via(BLACKHOLE.into())));
    assert!(client.validate().await.is_err(), "切黑洞后应失败");
    // 切回 Direct：重建后通
    client.reconfigure(&chat_cfg(&base, EffectiveProxy::Direct));
    client.validate().await.unwrap();
    // Responses 客户端同样三连切（responses 响应格式与 chat 不同，单独 mock）
    let rbase = spawn_fixed("200 OK", RESP_OK).await;
    let mut rclient = znaide_core::llm::responses::ResponsesClient::new(&responses_cfg(
        &rbase,
        EffectiveProxy::Direct,
    ))
    .unwrap();
    rclient.validate().await.unwrap();
    rclient.reconfigure(&responses_cfg(
        &rbase,
        EffectiveProxy::Via(BLACKHOLE.into()),
    ));
    assert!(
        rclient.validate().await.is_err(),
        "responses 切黑洞后应失败"
    );
    rclient.reconfigure(&responses_cfg(&rbase, EffectiveProxy::Direct));
    rclient.validate().await.unwrap();
    unlock_env(&dir);
}

/// E5 web_fetch：proxy_url=None + 黑洞 env → 内网 mock 通；
/// 反向黑洞快照 → 失败（web.rs 不再直读 env 的回归锁）
#[tokio::test]
async fn e5_web_fetch_snapshot_ignores_env() {
    let base = spawn_fixed("200 OK", "<html><body><p>hello proxy</p></body></html>").await;
    let page = base.replace("/v1", "/page");
    let (g, dir) = lock_env("e5");
    std::env::set_var("HTTPS_PROXY", BLACKHOLE);
    drop(g);
    let perm = znaide_core::permissions::Permission::new(
        znaide_core::permissions::Mode::BypassPermissions,
    );
    let cwd = std::env::temp_dir();
    let ctx = znaide_core::tools::ToolContext {
        cwd: &cwd,
        permission: &perm,
        session_id: "e5",
        cancel: None,
        events: None,
        proxy_url: None,
    };
    let out = znaide_core::tools::web::web_fetch(&ctx, &serde_json::json!({ "url": page }))
        .await
        .unwrap();
    assert!(out.contains("hello proxy"), "直连快照应无视黑洞 env: {out}");
    // 反向：快照给黑洞 → 失败（证明快照真生效）
    let ctx2 = znaide_core::tools::ToolContext {
        cwd: &cwd,
        permission: &perm,
        session_id: "e5",
        cancel: None,
        events: None,
        proxy_url: Some(BLACKHOLE.into()),
    };
    assert!(
        znaide_core::tools::web::web_fetch(&ctx2, &serde_json::json!({ "url": page }))
            .await
            .is_err(),
        "黑洞快照应失败"
    );
    unlock_env(&dir);
}

/// E6 更新客户端：http_client_with_proxy(None) 在黑洞 env 下仍通；
/// 黑洞版失败（证明参数真生效）
#[tokio::test]
async fn e6_update_client_direct_ignores_env() {
    let base = spawn_fixed("200 OK", r#"{"tag_name":"v9.9.9"}"#).await;
    let (g, dir) = lock_env("e6");
    std::env::set_var("HTTPS_PROXY", BLACKHOLE);
    drop(g);
    let client = znaide_core::update::http_client_with_proxy(None).unwrap();
    let url = base.replace("/v1", "/releases/latest");
    let status = client.get(&url).send().await.unwrap().status();
    assert!(status.is_success(), "直连更新客户端应无视黑洞 env");
    // 黑洞版失败（证明参数真生效）
    let client2 = znaide_core::update::http_client_with_proxy(Some(BLACKHOLE)).unwrap();
    assert!(
        client2.get(&url).send().await.is_err(),
        "黑洞更新客户端应失败"
    );
    unlock_env(&dir);
}
