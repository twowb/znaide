/// SSE 增量解析器。关键点:一条事件(`data: ...` 行)可能被拆在多个 HTTP chunk 里,
/// 必须跨 chunk 缓冲,否则后半段(没有 `data:` 前缀)会被丢掉 → 工具 arguments 被截断。
/// 与协议无关:chat(`/chat/completions`)与 Responses(`/responses`)共用。
pub(crate) struct SseParser {
    buf: String,
}

impl SseParser {
    pub(crate) fn new() -> Self {
        Self { buf: String::new() }
    }

    /// 送入一块网络数据,返回其中完整事件的 `data:` 负载(已去前缀)。
    /// 事件以空行分隔;兼容 \n 与 \r\n。未闭合的部分留到下次。
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        // 先把换行统一成 \n(兼容 \r\n / \r),跨 chunk 的 \r\n 也能被下次拼上
        let text = String::from_utf8_lossy(chunk)
            .replace("\r\n", "\n")
            .replace('\r', "\n");
        self.buf.push_str(&text);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.find("\n\n") {
            let event = self.buf[..pos].to_string();
            self.buf.drain(..pos + 2);
            for line in event.lines() {
                if let Some(rest) = line.strip_prefix("data:") {
                    out.push(rest.trim().to_string());
                }
            }
        }
        out
    }

    /// 流结束时的残余数据(服务端可能不补最后一个空行)
    pub(crate) fn finish(&mut self) -> Vec<String> {
        if self.buf.trim().is_empty() {
            return Vec::new();
        }
        let event = std::mem::take(&mut self.buf);
        let mut out = Vec::new();
        for line in event.lines() {
            if let Some(rest) = line.strip_prefix("data:") {
                out.push(rest.trim().to_string());
            }
        }
        out
    }
}
