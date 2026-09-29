//! 弱网重试的端到端验证:重试只能发生在**一次模型调用**里面。
//!
//! 断言三件容易做错的事:①真的重发了请求、且次数就是配置的次数;
//! ②失败那次不占轮预算、不计 usage;③不值得重试的状态码(400)一次都不多发。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use znaide_core::config::{EffectiveProxy, Resolved};
use znaide_core::llm::OpenAiClient;
use znaide_core::permissions::Mode;
use znaide_core::session::{Session, SessionEvent};

fn json_body(content: &str) -> String {
    format!(
        r#"{{"choices":[{{"message":{{"role":"assistant","content":"{content}"}},"finish_reason":"stop"}}],"usage":{{"prompt_tokens":5,"completion_tokens":2}}}}"#
    )
}

fn sse_body(content: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{content}\"}}}}]}}\n\ndata: [DONE]\n\n"
    )
}

/// 假端点:第 i 次请求回 `script[i]`(越界就用最后一条),并记下总请求数。
/// 请求体里带 `"stream":true` 时回 SSE,否则回普通 JSON —— 流式与非流式两条路径
/// 共用同一个重试循环,两边都要能验。
async fn endpoint(script: Vec<(u16, &'static str)>) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let script = script.clone();
            let h = h.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 16384];
                let n = match sock.read(&mut buf).await {
                    Ok(n) => n,
                    Err(_) => return,
                };
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let idx = h.fetch_add(1, Ordering::SeqCst);
                let (status, content) = script
                    .get(idx)
                    .or_else(|| script.last())
                    .copied()
                    .unwrap_or((200, ""));
                let stream = req.contains("\"stream\":true");
                let (ctype, payload) = if status == 200 && stream {
                    ("text/event-stream", sse_body(content))
                } else if status == 200 {
                    ("application/json", json_body(content))
                } else {
                    ("application/json", content.to_string())
                };
                let head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    payload.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(payload.as_bytes()).await;
                let _ = sock.flush().await;
            });
        }
    });
    (format!("http://{addr}/v1"), hits)
}

async fn session_at(base_url: &str, events: bool, retry: usize) -> (Session, tokio::sync::mpsc::UnboundedReceiver<SessionEvent>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let cfg = Resolved {
        model: "m".into(),
        base_url: base_url.to_string(),
        api_key: None,
        provider_name: "mock".into(),
        context_window: None,
        // 测试自己就是端点:显式直连,免得开发机上的 HTTPS_PROXY 把请求带跑偏
        proxy: EffectiveProxy::Direct,
    };
    let llm = OpenAiClient::new(&cfg).unwrap();
    let cwd = std::env::temp_dir().join(format!("znaide_retry_{}_{}", std::process::id(), retry));
    let mut s = Session::new(
        llm,
        cwd,
        Mode::BypassPermissions,
        if events { Some(tx) } else { None },
        CancellationToken::new(),
        false,
        None,
        None,
        "",
    )
    .unwrap();
    s.set_retry(retry);
    (s, rx)
}

/// 500 之后成功:重发一次、最终文本正确;失败那次不落历史也不计 usage
#[tokio::test]
async fn retries_a_retryable_error_then_succeeds() {
    let (base, hits) = endpoint(vec![(500, "boom"), (200, "pong")]).await;
    let (mut s, _rx) = session_at(&base, false, 3).await;

    let r = s.run_turn("你好").await.unwrap();
    assert_eq!(r.text, "pong");
    assert_eq!(hits.load(Ordering::SeqCst), 2, "应当恰好重发一次");
    // 只有成功那次计 usage(失败那次不该混进来)
    assert_eq!(r.input_tokens, 5);
    assert_eq!(r.output_tokens, 2);
}

/// 默认关:不设 retry 时,5xx 直接失败、一次都不多发(与旧行为一致)
#[tokio::test]
async fn no_retry_by_default() {
    let (base, hits) = endpoint(vec![(500, "boom")]).await;
    let (mut s, _rx) = session_at(&base, false, 0).await;

    let e = s.run_turn("你好").await.err().expect("应当报错");
    assert!(format!("{e:#}").contains("500"), "{e:#}");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

/// 400 不值得重试:配置了重试也不多发一次
#[tokio::test]
async fn non_retryable_status_is_not_retried() {
    let (base, hits) = endpoint(vec![(400, "bad request")]).await;
    let (mut s, _rx) = session_at(&base, false, 3).await;

    assert!(s.run_turn("你好").await.is_err());
    assert_eq!(hits.load(Ordering::SeqCst), 1, "400 不该重试");
}

/// 空回复(200 但没有任何内容)也算弱网表现:重发后拿到内容
#[tokio::test]
async fn empty_reply_is_retried() {
    let (base, hits) = endpoint(vec![(200, ""), (200, "pong")]).await;
    let (mut s, _rx) = session_at(&base, false, 2).await;

    let r = s.run_turn("你好").await.unwrap();
    assert_eq!(r.text, "pong");
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

/// 重试用尽仍失败:请求次数 = 1 + retry
#[tokio::test]
async fn exhausts_retries_and_fails() {
    let (base, hits) = endpoint(vec![(503, "unavailable")]).await;
    let (mut s, _rx) = session_at(&base, false, 2).await;

    assert!(s.run_turn("你好").await.is_err());
    assert_eq!(hits.load(Ordering::SeqCst), 3, "1 次原始 + 2 次重试");
}

/// 流式路径:重试时发 LlmRetrying(UI 据此把半截输出撤掉),轮预算不重置
#[tokio::test]
async fn stream_emits_llm_retrying_without_extra_rounds() {
    let (base, hits) = endpoint(vec![(500, "boom"), (200, "pong")]).await;
    let (mut s, mut rx) = session_at(&base, true, 2).await;

    let r = s.run_turn("你好").await.unwrap();
    assert_eq!(r.text, "pong");
    assert_eq!(hits.load(Ordering::SeqCst), 2);

    let mut retrying = 0;
    let mut rounds = 0;
    let mut finished = 0;
    while let Ok(ev) = rx.try_recv() {
        match ev {
            SessionEvent::LlmRetrying { attempt, max, .. } => {
                assert_eq!((attempt, max), (1, 2));
                retrying += 1;
            }
            SessionEvent::RoundStarted { .. } => rounds += 1,
            SessionEvent::TurnFinished { .. } => finished += 1,
            _ => {}
        }
    }
    assert_eq!(retrying, 1, "重试要通知 UI");
    assert_eq!(rounds, 1, "重试不占轮预算");
    assert_eq!(finished, 1);
}
