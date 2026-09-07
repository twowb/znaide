//! 网页抓取:把 URL 内容转为纯文本给模型阅读
use super::{ToolContext, ToolError, ToolOutput};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
struct WebArgs {
    url: String,
    #[serde(default)]
    max_chars: Option<usize>,
}

pub async fn web_fetch(ctx: &ToolContext<'_>, args: &Value) -> Result<ToolOutput, ToolError> {
    let _ = ctx;
    let a: WebArgs = serde_json::from_value(args.clone())?;
    if !(a.url.starts_with("http://") || a.url.starts_with("https://")) {
        return Err(ToolError("url 必须以 http:// 或 https:// 开头".into()));
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("znaide/0.1 (terminal AI assistant)")
        .build()
        .map_err(|e| ToolError(format!("HTTP 客户端初始化失败: {e}")))?;

    let resp = client
        .get(&a.url)
        .send()
        .await
        .map_err(|e| ToolError(format!("请求 {url} 失败: {e}", url = a.url)))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(ToolError(format!("HTTP {status}")));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| ToolError(format!("读取响应失败: {e}")))?;
    let text = String::from_utf8_lossy(&bytes);
    let plain = html_to_text(&text);

    let max = a.max_chars.unwrap_or(8000);
    let out = truncate(&plain, max);
    Ok(format!(
        "抓取 {url}(HTTP {status},{n} 字符):\n\n{out}",
        url = a.url,
        n = plain.chars().count()
    ))
}

/// 简易 HTML→文本:去 script/style/head 内容、去标签、解常见实体
fn html_to_text(html: &str) -> String {
    // 先整段剔除不需要的区块内容(script/style/head/noscript)
    let mut text = html.to_string();
    for tag in ["script", "style", "head", "noscript", "svg"] {
        text = strip_block(&text, tag);
    }

    let mut out = String::with_capacity(text.len());
    let mut in_tag = false;
    for c in text.chars() {
        if in_tag {
            if c == '>' {
                in_tag = false;
            }
            continue;
        }
        if c == '<' {
            in_tag = true;
        } else {
            out.push(c);
        }
    }

    // 解实体
    let out = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&ndash;", "–")
        .replace("&mdash;", "—");

    // 折叠多余空行
    let mut lines: Vec<String> = Vec::new();
    let mut blank = 0;
    for l in out.lines() {
        let t = l.trim();
        if t.is_empty() {
            blank += 1;
            if blank <= 1 {
                lines.push(String::new());
            }
        } else {
            blank = 0;
            lines.push(t.to_string());
        }
    }
    lines.join("\n")
}

/// 删除 <tag>...</tag> 及其内容(不区分大小写,不嵌套处理,char 安全)
fn strip_block(html: &str, tag: &str) -> String {
    let open_tag = format!("<{tag}");
    let close_tag = format!("</{tag}>");
    let chars: Vec<char> = html.chars().collect();
    let mut out = String::with_capacity(html.len());
    let mut i = 0;
    while i < chars.len() {
        // 检查当前位置是否以 open 开头(大小写不敏感)
        if chars[i] == '<' {
            let window: String = chars[i..].iter().take(open_tag.len() + 4).collect();
            let wl = window.to_lowercase();
            if wl.starts_with(&open_tag) {
                // 跳过直到本标签的 '>'
                let mut j = i;
                while j < chars.len() && chars[j] != '>' {
                    j += 1;
                }
                if j < chars.len() {
                    i = j + 1;
                }
                // 跳过内容直到 </tag>
                let mut done = false;
                while i < chars.len() && !done {
                    let win: String = chars[i..].iter().take(close_tag.len() + 2).collect();
                    if win.to_lowercase().starts_with(&close_tag) {
                        i += close_tag.chars().count();
                        done = true;
                    } else {
                        i += 1;
                    }
                }
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}\n…(已截断)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_blocks_and_tags() {
        let html = "<html><head><style>.x{}</style></head><body><h1>标题</h1><p>正文 <b>粗</b></p><script>alert(1)</script></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("标题"));
        assert!(text.contains("正文"));
        assert!(text.contains("粗"));
        assert!(!text.contains("alert"));
        assert!(!text.contains(".x"));
    }
}
