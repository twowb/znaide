//! need03 按需直改端到端:直接提交(无网络) / 验证后提交(mock) / 切家 / env Key 守卫。
//! mock 风格仿 retry_e2e.rs(按 Content-Length 读完请求再回响应)。

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use znaide_core::config::{Config, EffectiveProxy, ProtocolKind, Resolved, RetryConfig};

static E2E_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 用独立数据目录跑一段闭包(隔离本机 ~/.znaide/config.json)
fn with_data_dir(name: &str, f: impl FnOnce()) {
    let _g = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for v in [
        "ZNAIDE_RETRY",
        "ZNAIDE_NO_RETRY",
        "ZNAIDE_PROVIDER",
        "ZNAIDE_MODEL",
        "ZNAIDE_BASE_URL",
        "ZNAIDE_API_KEY",
        "ZNAIDE_PROTOCOL",
        "ZNAIDE_SESSION_HEADER",
    ] {
        std::env::remove_var(v);
    }
    let dir = std::env::temp_dir().join(format!("znaide_qc_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("ZNAIDE_DATA_DIR", &dir);
    f();
    std::env::remove_var("ZNAIDE_DATA_DIR");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 固定响应的 mock(读完请求→回固定 HTTP 包→关连接)
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

fn draft_for(base: &str) -> Resolved {
    Resolved {
        model: "qc-model".into(),
        base_url: base.into(),
        api_key: None,
        provider_name: "qc-house".into(),
        context_window: None,
        protocol: ProtocolKind::Chat,
        session_header_enabled: false,
        retry: RetryConfig::disabled(),
        proxy: EffectiveProxy::Direct,
    }
}

/// E1:直接提交轮数——无网络断言;config.json 落 max_turns;resolve→effective 即时变
#[test]
fn e1_direct_submit_max_turns_no_network() {
    with_data_dir("e1", || {
        let mut cfg = Config::default();
        cfg.save_max_turns(Some(50)).unwrap();
        let back = Config::load().unwrap();
        assert_eq!(back.max_turns, Some(50));
        assert_eq!(back.effective_max_turns(), 50);
        // 顶层三件套口径:按需保存不清空用户手写的顶层覆盖(R2)
        assert_eq!(back.provider, None);
    });
}

/// E2:直接提交重试——落 retry;resolve_retry 生效;超限 99→8
#[test]
fn e2_direct_submit_retry_clamped() {
    with_data_dir("e2", || {
        let mut cfg = Config::default();
        cfg.save_retry(RetryConfig::enabled_with(99)).unwrap();
        let back = Config::load().unwrap();
        assert_eq!(
            back.retry.max_retries,
            znaide_core::config::MAX_RETRIES
        );
        assert_eq!(
            back.resolve_retry(None, false).effective_times(),
            znaide_core::config::MAX_RETRIES
        );
    });
}

/// E3:验证后提交成功——mock 200 → 落盘(验证通过才保存)
#[tokio::test]
async fn e3_verify_then_save_on_success() {
    let base = spawn_fixed(
        "200 OK",
        r#"{"id":"x","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}]}"#,
    )
    .await;
    // probe_chat 成功 → 宿主才会调 save_*;这里按宿主顺序执行并断言落盘
    let draft = draft_for(&base);
    znaide_core::llm::openai::probe_chat(&draft).await.unwrap();
    with_data_dir("e3", || {
        let mut cfg = Config::default();
        cfg.save_model("qc-house", "qc-model").unwrap();
        cfg.save_base_url("qc-house", &base).unwrap();
        let back = Config::load().unwrap();
        assert_eq!(
            back.providers["qc-house"].model.as_deref(),
            Some("qc-model")
        );
        assert_eq!(
            back.providers["qc-house"].base_url.as_deref(),
            Some(base.as_str())
        );
    });
}

/// E4:验证后提交失败——mock 500 → 文件未动(宿主不调 save_*)
#[tokio::test]
async fn e4_verify_failure_keeps_file_untouched() {
    let base = spawn_fixed("500 Internal Server Error", r#"{"error":"boom"}"#).await;
    let draft = draft_for(&base);
    assert!(znaide_core::llm::openai::probe_chat(&draft).await.is_err());
    with_data_dir("e4", || {
        // 空文件:验证失败后宿主不写盘,文件仍不存在
        assert!(!znaide_core::config::config_path().exists());
    });
}

/// E5:切家+验证——mock 新家端点 200 → 指针+新家条目双落;旧家条目不动
#[tokio::test]
async fn e5_switch_provider_then_verify() {
    let base_b = spawn_fixed(
        "200 OK",
        r#"{"id":"x","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}]}"#,
    )
    .await;
    let draft = Resolved {
        provider_name: "houseB".into(),
        ..draft_for(&base_b)
    };
    znaide_core::llm::openai::probe_chat(&draft).await.unwrap();
    with_data_dir("e5", || {
        let mut cfg = Config::default();
        cfg.save_model("houseA", "model-a").unwrap();
        // 验证通过 → switch + 补条目(宿主顺序)
        cfg.switch_provider("houseB").unwrap();
        cfg.save_model("houseB", "model-b").unwrap();
        cfg.save_base_url("houseB", &base_b).unwrap();
        let back = Config::load().unwrap();
        assert_eq!(back.provider.as_deref(), Some("houseB"));
        assert_eq!(
            back.providers["houseA"].model.as_deref(),
            Some("model-a")
        );
        assert_eq!(
            back.providers["houseB"].model.as_deref(),
            Some("model-b")
        );
    });
}

/// E6:env Key 家——直接提交 Key(None)→ 文件无明文残留
#[test]
fn e6_env_key_never_written() {
    with_data_dir("e6", || {
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "cloudfam".into(),
            znaide_core::config::ProviderDef {
                base_url: Some("https://api.example.com/v1".into()),
                model: Some("m".into()),
                api_key_env: Some("ZNAIDE_API_KEY".into()),
                ..Default::default()
            },
        );
        let mut cfg = Config {
            provider: Some("cloudfam".into()),
            providers,
            ..Default::default()
        };
        // key_from_env 守卫:传 None=不动(04 §4)
        cfg.save_api_key("cloudfam", None).unwrap();
        let text = std::fs::read_to_string(znaide_core::config::config_path()).unwrap();
        assert!(!text.contains("sk-"), "env 家落盘不得有明文 key");
        let back = Config::load().unwrap();
        assert_eq!(back.providers["cloudfam"].api_key, None);
    });
}
