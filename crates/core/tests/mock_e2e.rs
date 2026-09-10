//! 集成测试:本地 mock OpenAI 兼容端点,验证 SSE 流式解析与会话工具循环。
//! mock 服务器读取健壮:按 Content-Length 精确读请求,响应后关闭连接。

use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::{timeout, Duration};
use znaide_core::config::Resolved;
use znaide_core::llm::openai::StreamEvent;
use znaide_core::llm::{ChatMessage, OpenAiClient, Usage};
use znaide_core::permissions::Mode;
use znaide_core::session::Session;
use tokio_util::sync::CancellationToken;

/// 启动 mock:每个连接读完整请求(header + content-length body),调用 handler 生成响应
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
            let Ok((mut sock, _)) = listener.accept().await else { break };
            let counter = counter.clone();
            let handler = handler.clone();
            tokio::spawn(async move {
                let _ = timeout(Duration::from_secs(10), async {
                    // 读 header(直到 \r\n\r\n)
                    let mut head = Vec::new();
                    let mut tmp = [0u8; 1024];
                    let mut header_end: Option<usize> = None;
                    while header_end.is_none() {
                        let n = sock.read(&mut tmp).await?;
                        if n == 0 {
                            return Ok::<(), std::io::Error>(());
                        }
                        head.extend_from_slice(&tmp[..n]);
                        header_end = find_header_end(&head);
                    }
                    let he = header_end.unwrap();
                    let head_str = String::from_utf8_lossy(&head[..he]);
                    // 解析 content-length
                    let content_length: usize = head_str
                        .lines()
                        .find_map(|l| {
                            let l = l.trim();
                            l.strip_prefix("Content-Length:")
                                .or_else(|| l.strip_prefix("content-length:"))
                                .map(|v| v.trim().parse().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    // 读足 body
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

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
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
    let mut out = String::from(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
    );
    // 分两块发送模拟流式(在响应体内分两次写需要 keep-alive;此处简单整体发,
    // 但每事件一行,parse_sse 逐行处理能力由单元测试覆盖)
    out.push_str(&payload);
    out.into_bytes()
}

#[tokio::test]
async fn sse_stream_accumulates_text() {
    let base = spawn_mock(|_n, _body| {
        sse_resp(&[
            r#"{"choices":[{"delta":{"content":"你好"},"finish_reason":null}]}"#,
            r#"{"choices":[{"delta":{"content":"世界"},"finish_reason":null}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":20,"completion_tokens":2,"total_tokens":22}}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ])
    })
    .await;

    let cfg = Resolved {
        model: "m".into(),
        base_url: base,
        api_key: None,
        provider_name: "mock".into(),
        context_window: None,
    };
    let client = OpenAiClient::new(&cfg).unwrap();
    let mut deltas: Vec<String> = Vec::new();
    let reply = client
        .chat_stream(&[ChatMessage::user("hi")], None, |ev| match ev {
            StreamEvent::TextDelta(t) => deltas.push(t),
            StreamEvent::ReasoningDelta(_) => {}
        })
        .await
        .unwrap();
    assert_eq!(reply.content.as_deref(), Some("你好世界"));
    assert_eq!(deltas.join(""), "你好世界");
    // 流末尾 usage chunk 被解析并带回
    assert_eq!(
        reply.usage,
        Usage {
            prompt_tokens: 20,
            completion_tokens: 2
        }
    );
}

#[tokio::test]
async fn sse_stream_accumulates_tool_calls() {
    let base = spawn_mock(|_n, _body| {
        sse_resp(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"read_file","arguments":"{\"path\":"}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"/tmp/x\"}"}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ])
    })
    .await;

    let cfg = Resolved {
        model: "m".into(),
        base_url: base,
        api_key: None,
        provider_name: "mock".into(),
        context_window: None,
    };
    let client = OpenAiClient::new(&cfg).unwrap();
    let reply = client
        .chat_stream(&[ChatMessage::user("hi")], None, |_| {})
        .await
        .unwrap();
    assert_eq!(reply.tool_calls.len(), 1);
    assert_eq!(reply.tool_calls[0].function.name, "read_file");
    assert_eq!(reply.tool_calls[0].function.arguments, r#"{"path":"/tmp/x"}"#);
}

#[tokio::test]
async fn session_run_turn_executes_tools() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_clone = calls.clone();
    let base = spawn_mock(move |_n, body| {
        let has_tool_result = body
            .get("messages")
            .and_then(|m| m.as_array())
            .map(|msgs| {
                msgs.iter()
                    .any(|m| m.get("role").and_then(|r| r.as_str()) == Some("tool"))
            })
            .unwrap_or(false);
        if !has_tool_result {
            http_json_resp(
                r#"{"id":"1","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","tool_calls":[{"id":"call_1","type":"function","function":{"name":"list_directory","arguments":"{\"path\":\"src\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            )
        } else {
            calls_clone.fetch_add(1, Ordering::SeqCst);
            http_json_resp(
                r#"{"id":"2","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"已完成侦查"},"finish_reason":"stop"}],"usage":{"prompt_tokens":50,"completion_tokens":7,"total_tokens":57}}"#,
            )
        }
    })
    .await;

    let cwd = std::env::temp_dir().join(format!("znaide_test_{}", std::process::id()));
    std::fs::create_dir_all(cwd.join("src")).unwrap();
    std::fs::write(cwd.join("src/a.rs"), "fn main() {}").unwrap();

    let cfg = Resolved {
        model: "m".into(),
        base_url: base,
        api_key: None,
        provider_name: "mock".into(),
        context_window: None,
    };
    let llm = OpenAiClient::new(&cfg).unwrap();
    let mut session = Session::new(
        llm,
        cwd.clone(),
        Mode::BypassPermissions,
        None,
        CancellationToken::new(),
        false, // persist=false:测试不写历史
        Some("mock-e2e-test".into()),
        None,  // 无 MCP
        "",    // 无人格
    )
    .unwrap();
    let result = session.run_turn("看看 src 目录").await.unwrap();
    assert_eq!(result.text, "已完成侦查");
    assert!(result.tool_calls >= 1);
    // 最终轮响应带 usage → 回合累计真实输入/输出(第一轮工具响应无 usage 不计数)
    assert_eq!(result.input_tokens, 50);
    assert_eq!(result.output_tokens, 7);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    std::fs::remove_dir_all(cwd).ok();
}

/// /clear 语义:清空上下文与历史文件后,后续请求不再携带旧消息(只 system+新 user),
/// jsonl 同步截断,只保留清空后的消息。
#[tokio::test]
async fn session_clear_context_wipes_messages_and_history() {
    let lens = Arc::new(std::sync::Mutex::new(Vec::new()));
    let lens_clone = lens.clone();
    let base = spawn_mock(move |_n, body| {
        let len = body
            .get("messages")
            .and_then(|m| m.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        lens_clone.lock().unwrap().push(len);
        http_json_resp(
            r#"{"id":"x","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"收到"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}"#,
        )
    })
    .await;

    let cwd = std::env::temp_dir().join(format!("znaide_clear_{}", std::process::id()));
    std::fs::create_dir_all(&cwd).unwrap();

    let cfg = Resolved {
        model: "m".into(),
        base_url: base,
        api_key: None,
        provider_name: "mock".into(),
        context_window: None,
    };
    let llm = OpenAiClient::new(&cfg).unwrap();
    let sid = format!("clear_test_{}", std::process::id());
    let mut session = Session::new(
        llm,
        cwd.clone(),
        Mode::BypassPermissions,
        None,
        CancellationToken::new(),
        true, // persist:写入真实历史文件以验证截断
        Some(sid.clone()),
        None,
        "",
    )
    .unwrap();

    let r1 = session.run_turn("第一轮问题").await.unwrap();
    assert_eq!(r1.text, "收到");
    let hp = session.history_path().unwrap().to_path_buf();
    assert!(std::fs::read_to_string(&hp).unwrap().contains("第一轮问题"));

    session.clear_context();

    let r2 = session.run_turn("第二轮问题").await.unwrap();
    assert_eq!(r2.text, "收到");
    // 两次请求的 messages 都是 system + 1 条 user:旧消息没有被带进第二轮
    let lens = lens.lock().unwrap();
    assert_eq!(lens.len(), 2);
    assert_eq!(lens[0], 2);
    assert_eq!(lens[1], 2);
    drop(lens);
    // jsonl 已截断:只剩第二轮的消息
    let text = std::fs::read_to_string(&hp).unwrap();
    assert!(!text.contains("第一轮问题"));
    assert!(text.contains("第二轮问题"));

    std::fs::remove_file(&hp).ok();
    std::fs::remove_dir_all(cwd).ok();
}

/// 会话历史"懒创建":没对话前不落盘;首次回合才建文件。
/// 无头(events=None)会话首行写入 headless 元数据;load_history 跳过 meta 行正常恢复。
#[tokio::test]
async fn headless_session_lazy_file_meta_and_resume() {
    let base = spawn_mock(|_n, _body| {
        http_json_resp(
            r#"{"id":"x","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"收到"},"finish_reason":"stop"}]}"#,
        )
    })
    .await;

    let cwd = std::env::temp_dir().join(format!("znaide_meta_{}", std::process::id()));
    std::fs::create_dir_all(&cwd).unwrap();
    let cfg = Resolved {
        model: "m".into(),
        base_url: base,
        api_key: None,
        provider_name: "mock".into(),
        context_window: None,
    };
    let sid = format!("meta_test_{}", std::process::id());
    let hp = znaide_core::config::data_dir()
        .join("sessions")
        .join(format!("{sid}.jsonl"));

    let llm = OpenAiClient::new(&cfg).unwrap();
    let mut session = Session::new(
        llm,
        cwd.clone(),
        Mode::BypassPermissions,
        None, // events=None → 无头
        CancellationToken::new(),
        true,
        Some(sid.clone()),
        None,
        "",
    )
    .unwrap();
    // 懒创建:刚建会话、尚未对话 → 文件还不存在
    assert!(!hp.exists(), "未对话前不应创建会话文件");

    let r1 = session.run_turn("你好").await.unwrap();
    assert_eq!(r1.text, "收到");
    assert!(hp.exists());
    let text = std::fs::read_to_string(&hp).unwrap();
    let first: serde_json::Value =
        serde_json::from_str(text.lines().next().unwrap_or("")).unwrap();
    assert_eq!(first["meta"]["headless"], true, "无头会话首行应有 headless 标记");
    assert_eq!(
        first["meta"]["schema"], "c4a23d6e50c7",
        "无头会话首行同样带历史格式版本"
    );

    // 恢复:meta 行被跳过,消息正常加载
    let llm2 = OpenAiClient::new(&cfg).unwrap();
    let mut sess2 = Session::new(
        llm2,
        cwd.clone(),
        Mode::BypassPermissions,
        None,
        CancellationToken::new(),
        false,
        None,
        None,
        "",
    )
    .unwrap();
    let loaded = sess2.load_history(&hp).unwrap();
    assert!(loaded >= 2, "应加载 user+assistant 两条,got {loaded}");

    std::fs::remove_file(&hp).ok();
    std::fs::remove_dir_all(cwd).ok();
}

/// 交互会话(events=Some)历史文件首行同样写 meta:headless=false + schema 版本
#[tokio::test]
async fn interactive_session_writes_schema_meta() {
    let base = spawn_mock(|_n, _body| {
        http_json_resp(
            r#"{"id":"x","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"收到"},"finish_reason":"stop"}]}"#,
        )
    })
    .await;

    let cwd = std::env::temp_dir().join(format!("znaide_imeta_{}", std::process::id()));
    std::fs::create_dir_all(&cwd).unwrap();
    let cfg = Resolved {
        model: "m".into(),
        base_url: base,
        api_key: None,
        provider_name: "mock".into(),
        context_window: None,
    };
    let sid = format!("imeta_test_{}", std::process::id());
    let hp = znaide_core::config::data_dir()
        .join("sessions")
        .join(format!("{sid}.jsonl"));

    let llm = OpenAiClient::new(&cfg).unwrap();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<znaide_core::session::SessionEvent>();
    let mut session = Session::new(
        llm,
        cwd.clone(),
        Mode::BypassPermissions,
        Some(tx),
        CancellationToken::new(),
        true,
        Some(sid.clone()),
        None,
        "",
    )
    .unwrap();
    session.run_turn("你好").await.unwrap();
    let first = std::fs::read_to_string(&hp)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_string();
    let v: serde_json::Value = serde_json::from_str(&first).unwrap();
    assert_eq!(v["meta"]["headless"], false, "交互会话不应标 headless");
    assert_eq!(v["meta"]["schema"], "c4a23d6e50c7", "meta 应带历史格式版本");

    std::fs::remove_file(&hp).ok();
    std::fs::remove_dir_all(cwd).ok();
}

/// 历史格式版本异源(meta schema ≠ 当前):恢复时发出提示;当前版本无提示
#[tokio::test]
async fn foreign_schema_in_history_emits_notice() {
    let base = spawn_mock(|_n, _body| {
        http_json_resp(
            r#"{"id":"x","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"收到"},"finish_reason":"stop"}]}"#,
        )
    })
    .await;
    let cwd = std::env::temp_dir().join(format!("znaide_fschema_{}", std::process::id()));
    std::fs::create_dir_all(&cwd).unwrap();
    let cfg = Resolved {
        model: "m".into(),
        base_url: base,
        api_key: None,
        provider_name: "mock".into(),
        context_window: None,
    };
    let hp = znaide_core::config::data_dir()
        .join("sessions")
        .join(format!("fschema_test_{}.jsonl", std::process::id()));
    // 手工写一份异源 schema 的历史
    std::fs::write(
        &hp,
        "{\"meta\":{\"headless\":false,\"schema\":\"deadbeef11\"}}\n\
         {\"role\":\"user\",\"content\":\"hi\"}\n\
         {\"role\":\"assistant\",\"content\":\"ok\"}\n",
    )
    .unwrap();

    let (tx, mut rx) =
        tokio::sync::mpsc::unbounded_channel::<znaide_core::session::SessionEvent>();
    let llm = OpenAiClient::new(&cfg).unwrap();
    let mut session = Session::new(
        llm,
        cwd.clone(),
        Mode::BypassPermissions,
        Some(tx),
        CancellationToken::new(),
        false,
        None,
        None,
        "",
    )
    .unwrap();
    let loaded = session.load_history(&hp).unwrap();
    assert_eq!(loaded, 2);
    let mut saw_notice = false;
    while let Ok(ev) = rx.try_recv() {
        if let znaide_core::session::SessionEvent::Notice(t) = ev {
            if t.contains("其他版本写入") {
                saw_notice = true;
            }
        }
    }
    assert!(saw_notice, "异源 schema 应提示版本不符");

    // 当前版本 schema → 无提示
    std::fs::write(
        &hp,
        "{\"meta\":{\"headless\":false,\"schema\":\"c4a23d6e50c7\"}}\n\
         {\"role\":\"user\",\"content\":\"hi\"}\n",
    )
    .unwrap();
    let (tx2, mut rx2) =
        tokio::sync::mpsc::unbounded_channel::<znaide_core::session::SessionEvent>();
    let llm2 = OpenAiClient::new(&cfg).unwrap();
    let mut session2 = Session::new(
        llm2,
        cwd.clone(),
        Mode::BypassPermissions,
        Some(tx2),
        CancellationToken::new(),
        false,
        None,
        None,
        "",
    )
    .unwrap();
    session2.load_history(&hp).unwrap();
    let mut any_notice = false;
    while let Ok(ev) = rx2.try_recv() {
        if let znaide_core::session::SessionEvent::Notice(_) = ev {
            any_notice = true;
        }
    }
    assert!(!any_notice, "当前版本 schema 不应提示");

    std::fs::remove_file(&hp).ok();
    std::fs::remove_dir_all(cwd).ok();
}

/// 回归(SSE 跨网络包截断):一条 `data:` 事件行被拆在两个 TCP 写入里,
/// 旧的无状态 parse_sse 会丢掉后半段 → 工具 arguments 被截断 / 工具调用丢失。
/// 这里用 HTTP chunked 真实地把一条事件行从中间切开再分两次发送。
#[tokio::test]
async fn tool_call_survives_sse_line_split_across_network_chunks() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { break };
            tokio::spawn(async move {
                let _ = timeout(Duration::from_secs(10), async {
                    // 读完请求 header + body
                    let mut head = Vec::new();
                    let mut tmp = [0u8; 2048];
                    while find_header_end(&head).is_none() {
                        let n = sock.read(&mut tmp).await?;
                        if n == 0 {
                            return Ok::<(), std::io::Error>(());
                        }
                        head.extend_from_slice(&tmp[..n]);
                    }
                    let he = find_header_end(&head).unwrap();
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

                    let event_json = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"run_shell_command","arguments":"{\"command\":\"echo chunked-ok\"}"}}]},"finish_reason":"tool_calls"}]}"#;
                    let data_line = format!("data: {event_json}");
                    // 在参数 JSON 字符串中间切开(模拟 TCP 分片)
                    let mid = data_line.find("echo").unwrap() + 4;
                    let (a, b) = data_line.split_at(mid);
                    let mut chunk = |payload: &str| {
                        format!("{:x}\r\n{payload}\r\n", payload.len()).into_bytes()
                    };

                    sock.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                    )
                    .await?;
                    // 第一次写入:半条 data 行(无换行)
                    sock.write_all(&chunk(a)).await?;
                    tokio::time::sleep(Duration::from_millis(60)).await;
                    // 第二次写入:后半条 + 事件空行 + [DONE]
                    let tail = format!("{b}\n\n");
                    sock.write_all(&chunk(&tail)).await?;
                    sock.write_all(b"0\r\n\r\n").await?;
                    let _ = sock.shutdown().await;
                    Ok::<(), std::io::Error>(())
                })
                .await;
            });
        }
    });

    let cfg = Resolved {
        model: "m".into(),
        base_url: format!("http://{addr}/v1"),
        api_key: None,
        provider_name: "mock".into(),
        context_window: None,
    };
    let client = OpenAiClient::new(&cfg).unwrap();
    let reply = client
        .chat_stream(&[ChatMessage::user("hi")], None, |_| {})
        .await
        .unwrap();
    // 跨包截断后工具调用必须完整保留,arguments 一字不差
    assert_eq!(reply.tool_calls.len(), 1);
    assert_eq!(reply.tool_calls[0].function.name, "run_shell_command");
    assert_eq!(
        reply.tool_calls[0].function.arguments,
        r#"{"command":"echo chunked-ok"}"#
    );
}
/// 回归:模型输出空/`null` 的工具参数(弱模型通病)时,工具循环不能断。
/// - list_directory 空参数 → path 可选,应正常列出目录
/// - run_shell_command 参数为字符串 "null" → 归一化为 {} → 返回带指引的错误,模型可继续
#[tokio::test]
async fn empty_and_null_tool_arguments_keep_loop_alive() {
    let tool_results: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let seen = tool_results.clone();
    let base = spawn_mock(move |n, body| {
        // 记录每次请求里出现的 tool 结果内容(即工具执行后的回填)
        if let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) {
            for m in msgs {
                if m.get("role").and_then(|r| r.as_str()) == Some("tool") {
                    if let Some(c) = m.get("content").and_then(|c| c.as_str()) {
                        seen.lock().unwrap().push(c.to_string());
                    }
                }
            }
        }
        match n {
            // 第 1 次:list_directory,arguments 是空字符串
            0 => http_json_resp(
                r#"{"id":"1","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","tool_calls":[{"id":"c1","type":"function","function":{"name":"list_directory","arguments":""}}]},"finish_reason":"tool_calls"}]}"#,
            ),
            // 第 2 次:run_shell_command,arguments 是字符串 "null"
            1 => http_json_resp(
                r#"{"id":"2","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","tool_calls":[{"id":"c2","type":"function","function":{"name":"run_shell_command","arguments":"null"}}]},"finish_reason":"tool_calls"}]}"#,
            ),
            _ => http_json_resp(
                r#"{"id":"3","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"参数问题已处理完毕"},"finish_reason":"stop"}]}"#,
            ),
        }
    })
    .await;

    let cwd = std::env::temp_dir().join(format!("znaide_null_args_{}", std::process::id()));
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("hello.txt"), "x").unwrap();

    let cfg = Resolved {
        model: "m".into(),
        base_url: base,
        api_key: None,
        provider_name: "mock".into(),
        context_window: None,
    };
    let llm = OpenAiClient::new(&cfg).unwrap();
    let mut session = Session::new(
        llm,
        cwd.clone(),
        Mode::BypassPermissions,
        None,
        CancellationToken::new(),
        false,
        Some("mock-null-args".into()),
        None,
        "",
    )
    .unwrap();
    let result = session.run_turn("测试空参数工具调用").await.unwrap();
    assert_eq!(result.text, "参数问题已处理完毕");
    assert!(result.tool_calls >= 2, "应至少执行 2 次工具调用");

    let log = tool_results.lock().unwrap().clone();
    // list_directory 空参数:应正常执行(列出目录),而不是 "invalid type: null"
    assert!(
        log.iter().any(|c| c.contains("目录") && !c.contains("参数解析错误")),
        "list_directory 空参数应正常执行,实际: {log:?}"
    );
    // run_shell_command "null" 参数:归一化为 {} → 返回带指引的错误信息,循环继续
    assert!(
        log.iter()
            .any(|c| c.contains("command 不能为空") && c.contains("ls -la")),
        "空 command 应返回指引信息,实际: {log:?}"
    );
    assert!(
        !log.iter().any(|c| c.contains("invalid type: null")),
        "不能再出现 null 解析错误: {log:?}"
    );

    std::fs::remove_dir_all(cwd).ok();
}

/// 在临时 cwd 下建一个带 entry 脚本的项目级技能(demo)
#[cfg(unix)]
fn scaffold_demo_skill(cwd: &std::path::Path) {
    let dir = cwd.join(".znaide").join("skills").join("demo");
    std::fs::create_dir_all(dir.join("scripts")).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        "---\ndescription: 演示技能:按入口输出完成归档\nentry: scripts/run.sh\n---\n\
         查看入口脚本输出,按其中的文件清单完成任务。\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("scripts/run.sh"),
        "echo SKILL-MARKER-OK\nls -1 \"$SKILL_DIR\"\n",
    )
    .unwrap();
}

/// 回归:模型自主调用 skill 工具(name 走 enum)→ 收到渲染正文 + entry 输出 → 继续干活。
#[cfg(unix)]
#[tokio::test]
async fn model_invokes_skill_with_entry_script() {
    let tool_results: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let seen = tool_results.clone();
    let base = spawn_mock(move |n, body| {
        if let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) {
            for m in msgs {
                if m.get("role").and_then(|r| r.as_str()) == Some("tool") {
                    if let Some(c) = m.get("content").and_then(|c| c.as_str()) {
                        seen.lock().unwrap().push(c.to_string());
                    }
                }
            }
        }
        match n {
            0 => http_json_resp(
                r#"{"id":"1","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","tool_calls":[{"id":"c1","type":"function","function":{"name":"skill","arguments":"{\"name\":\"demo\",\"args\":\"\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            ),
            _ => http_json_resp(
                r#"{"id":"2","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"技能流程完成"},"finish_reason":"stop"}]}"#,
            ),
        }
    })
    .await;

    let cwd = std::env::temp_dir().join(format!("znaide_skill_e2e_{}", std::process::id()));
    std::fs::create_dir_all(&cwd).unwrap();
    scaffold_demo_skill(&cwd);

    let cfg = Resolved {
        model: "m".into(),
        base_url: base,
        api_key: None,
        provider_name: "mock".into(),
        context_window: None,
    };
    let llm = OpenAiClient::new(&cfg).unwrap();
    let mut session = Session::new(
        llm,
        cwd.clone(),
        Mode::BypassPermissions,
        None, // 无头
        CancellationToken::new(),
        false,
        Some("mock-skill".into()),
        None,
        "",
    )
    .unwrap();
    let result = session.run_turn("用 demo 技能处理").await.unwrap();
    assert_eq!(result.text, "技能流程完成");
    assert!(result.tool_calls >= 1);

    let log = tool_results.lock().unwrap().clone();
    assert!(log.len() >= 1, "skill 工具结果应回填给模型: {log:?}");
    // 正文(说明文字)已注入
    assert!(
        log[0].contains("查看入口脚本输出"),
        "应包含 SKILL.md 正文: {log:?}"
    );
    // entry 脚本真实执行且输出并入
    assert!(
        log[0].contains("SKILL-MARKER-OK") && log[0].contains("SKILL.md"),
        "entry 输出应并入工具结果: {log:?}"
    );

    std::fs::remove_dir_all(cwd).ok();
}

/// 手动触发不存在的技能:不开回合、不调模型、优雅返回(UI busy 状态靠空 TurnFinished 清掉)
#[tokio::test]
async fn manual_skill_unknown_name_fails_gracefully() {
    let cwd = std::env::temp_dir().join(format!("znaide_skill_none_{}", std::process::id()));
    std::fs::create_dir_all(&cwd).unwrap();

    let cfg = Resolved {
        model: "m".into(),
        base_url: "http://127.0.0.1:1/v1".into(), // 不应被连接
        api_key: None,
        provider_name: "mock".into(),
        context_window: None,
    };
    let llm = OpenAiClient::new(&cfg).unwrap();
    let mut session = Session::new(
        llm,
        cwd.clone(),
        Mode::BypassPermissions,
        None,
        CancellationToken::new(),
        false,
        Some("mock-skill-none".into()),
        None,
        "",
    )
    .unwrap();
    // 未知技能:run_skill_turn 应立刻返回(不发起任何网络请求),text 为空
    let r = session.run_skill_turn("no-such-skill", "").await.unwrap();
    assert!(r.text.is_empty());
    assert_eq!(r.tool_calls, 0);

    std::fs::remove_dir_all(cwd).ok();
}

/// /compact:旧消息压缩成摘要后,再次请求的 messages 显著变小,且含摘要注入。
#[tokio::test]
async fn compact_context_shrinks_next_request() {
    let last_len = Arc::new(std::sync::Mutex::new(0usize));
    let saw_summary = Arc::new(AtomicBool::new(false));
    let len2 = last_len.clone();
    let sum2 = saw_summary.clone();
    let base = spawn_mock(move |_n, body| {
        let msgs = body.get("messages").and_then(|m| m.as_array()).cloned().unwrap_or_default();
        // 压缩请求:只有 system(压缩器指令)+ user(待压缩对话)
        let is_compact = msgs.len() == 2
            && msgs[0]
                .get("content")
                .and_then(|c| c.as_str())
                .map(|c| c.contains("上下文压缩器"))
                .unwrap_or(false);
        if is_compact {
            return http_json_resp(
                r#"{"id":"c","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"摘要:已完成三项小任务,未完成事项待继续"},"finish_reason":"stop"}]}"#,
            );
        }
        *len2.lock().unwrap() = msgs.len();
        if msgs.get(1).and_then(|m| m.get("content")).and_then(|c| c.as_str())
            .map(|c| c.contains("【上下文摘要"))
            .unwrap_or(false)
        {
            sum2.store(true, Ordering::SeqCst);
        }
        http_json_resp(
            r#"{"id":"x","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"收到"},"finish_reason":"stop"}]}"#,
        )
    })
    .await;

    let cwd = std::env::temp_dir().join(format!("znaide_cmp_ctx_{}", std::process::id()));
    std::fs::create_dir_all(&cwd).unwrap();
    let cfg = Resolved {
        model: "m".into(),
        base_url: base,
        api_key: None,
        provider_name: "mock".into(),
        context_window: None,
    };
    let llm = OpenAiClient::new(&cfg).unwrap();
    let mut session = Session::new(
        llm,
        cwd.clone(),
        Mode::BypassPermissions,
        None,
        CancellationToken::new(),
        false,
        Some("mock-compact".into()),
        None,
        "",
    )
    .unwrap();
    // 积累 4 轮对话(每轮 2 条消息)
    for i in 0..4 {
        let r = session.run_turn(&format!("问题 {i}")).await.unwrap();
        assert_eq!(r.text, "收到");
    }
    let before = *last_len.lock().unwrap();
    assert!(before >= 8, "压缩前 messages 应已积累,got {before}");

    let removed = session.compact_context().await.unwrap();
    assert!(removed >= 2, "应压缩掉若干旧消息,got {removed}");

    let r = session.run_turn("压缩之后继续").await.unwrap();
    assert_eq!(r.text, "收到");
    let after = *last_len.lock().unwrap();
    // 摘要(1)+保留最近(≤4)+本回合新 user → 显著小于压缩前
    assert!(after < before, "压缩后请求应更小:before={before} after={after}");
    assert!(saw_summary.load(Ordering::SeqCst), "压缩后请求应含【上下文摘要】注入");

    std::fs::remove_dir_all(cwd).ok();
}

/// 回合请求失败(端点不可达)时:交互模式必须发出错误提示并复位忙态
/// (Notice + TurnFinished),不能只抛 Err 让 UI 永久卡在"正在思考与执行"。
#[tokio::test]
async fn session_turn_error_notifies_and_finishes() {
    use znaide_core::session::{describe_request_error, SessionEvent};

    let cfg = Resolved {
        model: "m".into(),
        base_url: "http://127.0.0.1:1/v1".into(), // 端口 1:立即拒绝连接
        api_key: None,
        provider_name: "mock".into(),
        context_window: None,
    };
    let llm = OpenAiClient::new(&cfg).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let cwd = std::env::temp_dir();
    let mut session = Session::new(
        llm,
        cwd.clone(),
        Mode::BypassPermissions,
        Some(tx),
        CancellationToken::new(),
        false,
        Some("mock-turn-err".into()),
        None,
        "",
    )
    .unwrap();

    // 错误本身保留并给出可读分类(连接失败/超时二选一)
    let err = match session.run_turn("你好").await {
        Err(e) => e,
        Ok(_) => panic!("指向不可达端点的回合应报错"),
    };
    let desc = describe_request_error(&err);
    assert!(
        desc.contains("无法连接") || desc.contains("超时"),
        "desc 应归类为连接类错误,got: {desc}"
    );

    // 事件流必须包含:错误 Notice + 复位忙态用的 TurnFinished
    let mut saw_notice = false;
    let mut saw_finish = false;
    while let Ok(ev) = rx.try_recv() {
        match ev {
            SessionEvent::Notice(_) => saw_notice = true,
            SessionEvent::TurnFinished { .. } => saw_finish = true,
            _ => {}
        }
    }
    assert!(saw_notice, "错误应以 Notice 告知 UI");
    assert!(saw_finish, "错误后应发 TurnFinished 复位忙态");
}

/// D1/D2 无头模式:模型一直要调工具、轮数预算用尽 → 不再"静默硬停",
/// 文本里说清"已达 N 轮"并给出 --max-turns 的调法(没有人可问,直接收尾)。
#[tokio::test]
async fn round_budget_exhausted_reports_headless() {
    let base = spawn_mock(|_n, _body| {
        http_json_resp(
            r#"{"id":"1","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","tool_calls":[{"id":"c1","type":"function","function":{"name":"list_directory","arguments":"{\"path\":\".\"}"}}]},"finish_reason":"tool_calls"}]}"#,
        )
    })
    .await;

    let cwd = std::env::temp_dir().join(format!("znaide_budget_{}", std::process::id()));
    std::fs::create_dir_all(&cwd).unwrap();
    let cfg = Resolved {
        model: "m".into(),
        base_url: base,
        api_key: None,
        provider_name: "mock".into(),
        context_window: None,
    };
    let llm = OpenAiClient::new(&cfg).unwrap();
    let mut session = Session::new(
        llm,
        cwd.clone(),
        Mode::BypassPermissions,
        None, // 无头:没有人可以问
        CancellationToken::new(),
        false,
        Some("mock-budget".into()),
        None,
        "",
    )
    .unwrap();
    session.set_max_turns(2);

    let result = session.run_turn("一直干活").await.unwrap();
    assert!(result.truncated, "轮数用尽应标记 truncated");
    assert_eq!(result.tool_calls, 2, "只该跑满 2 轮");
    assert!(
        result.text.contains("2 轮上限") && result.text.contains("--max-turns"),
        "提示要说清轮数与调法,got: {}",
        result.text
    );

    std::fs::remove_dir_all(cwd).ok();
}

/// D1:max_turns = 0 表示不限——模型连着调十几轮工具也不会被轮数上限打断,
/// 直到它自己给出无工具的回复。(每轮换一个文件,避免撞上 D3 的重复调用刹车)
#[tokio::test]
async fn max_turns_zero_means_unlimited() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_clone = calls.clone();
    let base = spawn_mock(move |_n, _body| {
        let i = calls_clone.fetch_add(1, Ordering::SeqCst);
        if i < 11 {
            http_json_resp(&format!(
                r#"{{"id":"{i}","object":"chat.completion","choices":[{{"index":0,"message":{{"role":"assistant","tool_calls":[{{"id":"c{i}","type":"function","function":{{"name":"read_file","arguments":"{{\"path\":\"src/f{i}.rs\"}}"}}}}]}},"finish_reason":"tool_calls"}}]}}"#
            ))
        } else {
            http_json_resp(
                r#"{"id":"end","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"收工"},"finish_reason":"stop"}]}"#,
            )
        }
    })
    .await;

    let cwd = std::env::temp_dir().join(format!("znaide_unlim_{}", std::process::id()));
    std::fs::create_dir_all(cwd.join("src")).unwrap();
    for i in 0..12 {
        std::fs::write(cwd.join(format!("src/f{i}.rs")), "fn main() {}").unwrap();
    }
    let cfg = Resolved {
        model: "m".into(),
        base_url: base,
        api_key: None,
        provider_name: "mock".into(),
        context_window: None,
    };
    let llm = OpenAiClient::new(&cfg).unwrap();
    let mut session = Session::new(
        llm,
        cwd.clone(),
        Mode::BypassPermissions,
        None,
        CancellationToken::new(),
        false,
        Some("mock-unlimited".into()),
        None,
        "",
    )
    .unwrap();
    session.set_max_turns(2); // 先用 2 验证钳制
    let capped = session.run_turn("先跑两轮").await.unwrap();
    assert!(capped.truncated, "max_turns=2 应被钳制");
    session.set_max_turns(0); // 再放开为不限
    let free = session.run_turn("这次不限轮数").await.unwrap();
    assert!(!free.truncated, "不限轮数时不该被截断: {}", free.text);
    assert!(free.text.contains("收工"));
    assert!(free.tool_calls >= 8, "应真的跑满 8 轮以上,got {}", free.tool_calls);

    std::fs::remove_dir_all(cwd).ok();
}

/// 轮次进度事件:每轮模型调用前发 `RoundStarted`(used/limit),状态栏据此显示"轮 x/y";
/// `max_turns = 0` 时 limit = 0(UI 显示 ∞)。只读工具不弹权限框,可在无人值守下跑完。
#[tokio::test]
async fn round_started_events_report_progress() {
    use znaide_core::session::SessionEvent;
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_clone = calls.clone();
    let base = spawn_mock(move |_n, _body| {
        let i = calls_clone.fetch_add(1, Ordering::SeqCst);
        // events=Some → 走流式通道,必须回 SSE
        if i < 3 {
            sse_resp(&[
                &format!(
                    r#"{{"choices":[{{"delta":{{"tool_calls":[{{"index":0,"id":"c{i}","type":"function","function":{{"name":"list_directory","arguments":"{{\"path\":\"src/f{i}\"}}"}}}}]}},"finish_reason":null}}]}}"#
                ),
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
                "[DONE]",
            ])
        } else {
            sse_resp(&[
                r#"{"choices":[{"delta":{"content":"收工"},"finish_reason":null}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
                "[DONE]",
            ])
        }
    })
    .await;

    let cwd = std::env::temp_dir().join(format!("znaide_rounds_{}", std::process::id()));
    std::fs::create_dir_all(cwd.join("src")).unwrap();
    let cfg = Resolved {
        model: "m".into(),
        base_url: base,
        api_key: None,
        provider_name: "mock".into(),
        context_window: None,
    };
    let llm = OpenAiClient::new(&cfg).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut session = Session::new(
        llm,
        cwd.clone(),
        Mode::BypassPermissions,
        Some(tx),
        CancellationToken::new(),
        false,
        Some("mock-rounds".into()),
        None,
        "",
    )
    .unwrap();

    // 有上限:每轮都要报 used/limit
    session.set_max_turns(50);
    session.run_turn("跑几轮").await.unwrap();
    let mut got: Vec<(usize, usize)> = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let SessionEvent::RoundStarted { used, limit } = ev {
            got.push((used, limit));
        }
    }
    assert_eq!(got, vec![(1, 50), (2, 50), (3, 50), (4, 50)], "每轮都要带上限");

    // 不限:limit = 0(UI 用 ∞ 表示)
    session.set_max_turns(0);
    session.run_turn("再来一次").await.unwrap();
    let mut last: Option<(usize, usize)> = None;
    while let Ok(ev) = rx.try_recv() {
        if let SessionEvent::RoundStarted { used, limit } = ev {
            last = Some((used, limit));
        }
    }
    assert_eq!(last, Some((1, 0)), "不限时 limit 报 0");

    std::fs::remove_dir_all(cwd).ok();
}

/// D3:同一个 (工具, 参数) 反复调用 → 不烧到轮数上限,判"原地打转"提前收尾;
/// 中间会先在工具结果里插提醒(给了自救机会)。
#[tokio::test]
async fn repeated_identical_call_is_stopped() {
    let base = spawn_mock(|_n, _body| {
        http_json_resp(
            r#"{"id":"1","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","tool_calls":[{"id":"c1","type":"function","function":{"name":"list_directory","arguments":"{\"path\":\".\"}"}}]},"finish_reason":"tool_calls"}]}"#,
        )
    })
    .await;

    let cwd = std::env::temp_dir().join(format!("znaide_stuck_{}", std::process::id()));
    std::fs::create_dir_all(&cwd).unwrap();
    let cfg = Resolved {
        model: "m".into(),
        base_url: base,
        api_key: None,
        provider_name: "mock".into(),
        context_window: None,
    };
    let llm = OpenAiClient::new(&cfg).unwrap();
    let mut session = Session::new(
        llm,
        cwd.clone(),
        Mode::BypassPermissions,
        None,
        CancellationToken::new(),
        false,
        Some("mock-stuck".into()),
        None,
        "",
    )
    .unwrap();

    let result = session.run_turn("重复同一个调用").await.unwrap();
    assert!(result.truncated, "判打转应标记 truncated");
    assert!(
        result.text.contains("原地打转"),
        "应明确说检测到原地打转,got: {}",
        result.text
    );
    // 默认轮数上限 200,这里必须远早于它停下(6 次重复即中止)
    assert!(
        result.tool_calls <= 6,
        "应在第 6 次重复时中止,实际 {} 次",
        result.tool_calls
    );

    std::fs::remove_dir_all(cwd).ok();
}
