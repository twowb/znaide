//! `probe_models` 的 HTTP 行为:GET /v1/models → 解析 data[].id。
//! 用 Rust 起最小 HTTP/1.1 服务,避免依赖 Python mock 的 keep-alive 细节。

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use znaide_core::config::{EffectiveHeaders, EffectiveProxy};

#[tokio::test]
async fn probe_models_reads_openai_model_list() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 2048];
        let _ = socket.read(&mut buf).await;
        let body = r#"{"object":"list","data":[{"id":"m2"},{"id":"m1"}]}"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        socket.write_all(resp.as_bytes()).await.unwrap();
        socket.flush().await.unwrap();
    });

    let models = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        znaide_core::llm::openai::probe_models(
            &format!("http://{addr}/v1"),
            None,
            &EffectiveProxy::Direct,
            &EffectiveHeaders::default(),
        ),
    )
    .await
    .expect("探测不应挂住")
    .expect("探测应成功");

    assert_eq!(models, vec!["m1".to_string(), "m2".to_string()]);
    server.await.unwrap();
}
