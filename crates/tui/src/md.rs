//! 轻量 markdown 渲染:把模型输出的 markdown 文本解析为带样式的行。
//! 支持:围栏代码块、标题、引用、列表、分隔线、行内代码/粗体/斜体/链接。
//! 纯函数,按给定宽度做 CJK 感知的自动换行。
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Normal,
    Heading(u8),
    Quote,
    List,
    Hr,
    Code,
    Empty,
}

#[derive(Debug, Clone)]
struct Row {
    kind: Kind,
    text: String,
}

fn wc(c: char) -> u16 {
    // 近似 Unicode 宽度:CJK/全角/emoji 按 2 列
    if c.is_control() {
        return 0;
    }
    let cp = c as u32;
    if (0x1100..=0x115f).contains(&cp)
        || (0x2e80..=0xa4cf).contains(&cp)
        || (0xac00..=0xd7a3).contains(&cp)
        || (0xf900..=0xfaff).contains(&cp)
        || (0xfe30..=0xfe4f).contains(&cp)
        || (0xff00..=0xff60).contains(&cp)
        || (0xffe0..=0xffe6).contains(&cp)
        || (0x1f000..=0x1faff).contains(&cp)
        || (0x1f300..=0x1f64f).contains(&cp)
        || (0x2600..=0x27bf).contains(&cp)
    {
        2
    } else {
        1
    }
}

fn text_width(s: &str) -> u16 {
    s.chars().map(wc).sum()
}

/// 按显示宽度把文本切成若干段(不破坏字符)。
pub fn wrap_plain(text: &str, width: u16) -> Vec<String> {
    wrap_text(text, width)
}

/// 按显示宽度把文本切成若干段(不破坏字符)。输入内嵌的 \n 会强制分段。
fn wrap_text(text: &str, width: u16) -> Vec<String> {
    // 先剥控制字节(Reasoning/Notice 等纯文本路径也走这里,不进 render)
    let text = crate::ansi::strip(text);
    if width <= 2 {
        return text.split('\n').map(|s| s.to_string()).collect();
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0u16;
    for ch in text.chars() {
        if ch == '\n' {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
            continue;
        }
        let w = wc(ch);
        if cur_w + w > width && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        cur.push(ch);
        cur_w += w;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// 把原始 markdown 文本切分为逻辑行
fn parse(text: &str) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut in_code = false;
    for raw in text.lines() {
        let line = raw.trim_end_matches('\r');
        let trimmed = line.trim();
        if in_code {
            if trimmed.starts_with("```") {
                in_code = false;
                continue;
            }
            rows.push(Row { kind: Kind::Code, text: line.to_string() });
            continue;
        }
        if trimmed.starts_with("```") {
            in_code = true;
            rows.push(Row { kind: Kind::Empty, text: String::new() });
            continue;
        }
        if trimmed.is_empty() {
            rows.push(Row { kind: Kind::Empty, text: String::new() });
            continue;
        }
        // 标题
        if let Some(h) = parse_heading(trimmed) {
            rows.push(Row { kind: Kind::Heading(h), text: trimmed.to_string() });
            continue;
        }
        // 分隔线
        if trimmed.chars().all(|c| c == '-' || c == '=' || c == '*' || c == '_')
            && trimmed.chars().count() >= 3
            && trimmed.contains('*') == false
        {
            rows.push(Row { kind: Kind::Hr, text: String::new() });
            continue;
        }
        // 引用
        if let Some(rest) = trimmed.strip_prefix('>') {
            rows.push(Row {
                kind: Kind::Quote,
                text: rest.trim_start().to_string(),
            });
            continue;
        }
        // 列表(-、*、1. 等)
        if is_list_item(trimmed) {
            rows.push(Row { kind: Kind::List, text: line.to_string() });
            continue;
        }
        rows.push(Row { kind: Kind::Normal, text: line.to_string() });
    }
    if in_code {
        rows.push(Row { kind: Kind::Empty, text: String::new() });
    }
    rows
}

fn parse_heading(s: &str) -> Option<u8> {
    let hashes = s.chars().take_while(|c| *c == '#').count();
    if hashes >= 1 && hashes <= 6 {
        let rest = s[hashes..].trim_start();
        if rest.is_empty() {
            None
        } else {
            Some(hashes as u8)
        }
    } else {
        None
    }
}

fn is_list_item(s: &str) -> bool {
    if s.starts_with("- ") || s.starts_with("* ") || s.starts_with("+ ") {
        return true;
    }
    let mut chars = s.chars();
    let mut digits = 0;
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            digits += 1;
            continue;
        }
        return (c == '.' || c == ')') && digits > 0 && chars.next() == Some(' ');
    }
    false
}

// ---------- 行内样式 ----------

fn inline_style(kind: Kind) -> Style {
    match kind {
        Kind::Heading(_) => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        Kind::Quote => Style::default().fg(Color::Blue).add_modifier(Modifier::ITALIC),
        Kind::List => Style::default().fg(Color::White),
        Kind::Normal => Style::default().fg(Color::White),
        Kind::Code | Kind::Empty | Kind::Hr => Style::default().fg(Color::White),
    }
}

/// 行内 markdown 解析:**粗体** `代码` *斜体* [文本](链接)
fn inline(text: &str, base: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut plain = String::new();
    let flush = |plain: &mut String, spans: &mut Vec<Span<'static>>| {
        if !plain.is_empty() {
            spans.push(Span::styled(std::mem::take(plain), base));
        }
    };
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        // 行内代码 `...`
        if c == '`' {
            if let Some(end) = chars[i + 1..].iter().position(|&x| x == '`') {
                flush(&mut plain, &mut spans);
                let code: String = chars[i + 1..i + 1 + end].iter().collect();
                spans.push(Span::styled(
                    code,
                    base.patch(Style::default().fg(Color::Yellow)),
                ));
                i += end + 2;
                continue;
            }
        }
        // **粗体**
        if c == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
            if let Some(end_rel) = chars[i + 2..].windows(2).position(|w| w == ['*', '*']) {
                flush(&mut plain, &mut spans);
                let bold: String = chars[i + 2..i + 2 + end_rel].iter().collect();
                spans.push(Span::styled(
                    bold,
                    base.patch(Style::default().add_modifier(Modifier::BOLD)),
                ));
                i += end_rel + 4;
                continue;
            }
        }
        // *斜体*
        if c == '*' {
            if let Some(end_rel) = chars[i + 1..].iter().position(|&x| x == '*') {
                flush(&mut plain, &mut spans);
                let ital: String = chars[i + 1..i + 1 + end_rel].iter().collect();
                spans.push(Span::styled(
                    ital,
                    base.patch(Style::default().add_modifier(Modifier::ITALIC)),
                ));
                i += end_rel + 2;
                continue;
            }
        }
        plain.push(c);
        i += 1;
    }
    flush(&mut plain, &mut spans);
    spans
}

fn heading_marker(level: u8) -> String {
    if level <= 1 {
        "■ ".into()
    } else if level == 2 {
        "• ".into()
    } else {
        "· ".into()
    }
}

/// 渲染一块 markdown 文本,返回带样式的行(已按宽度换行)。
pub fn render(text: &str, width: u16) -> Vec<Line<'static>> {
    // 模型/用户文本可能内嵌真实 ANSI:先剥掉,避免原样渲染被终端执行
    let text = crate::ansi::strip(text);
    let rows = parse(&text);
    let mut out: Vec<Line<'static>> = Vec::new();
    for row in rows {
        match row.kind {
            Kind::Empty => {
                if !out.is_empty() {
                    out.push(Line::from(""));
                }
            }
            Kind::Hr => {
                let n = (width.saturating_sub(2) as usize).max(8);
                out.push(Line::from(Span::styled(
                    "─".repeat(n),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            Kind::Code => {
                // 代码块:每个物理行带代码样式,不打折进普通样式
                for seg in wrap_text(&row.text, width.saturating_sub(2)) {
                    out.push(Line::from(Span::styled(
                        seg,
                        Style::default().fg(Color::Yellow),
                    )));
                }
            }
            Kind::Heading(lv) => {
                let body = row.text.trim_start_matches('#').trim_start();
                let prefix = heading_marker(lv);
                let base = inline_style(Kind::Heading(lv));
                let content_w = width.saturating_sub(text_width(&prefix)).max(8);
                let mut spans: Vec<Span<'static>> = Vec::new();
                for (idx, seg) in wrap_text(body, content_w).iter().enumerate() {
                    if idx == 0 {
                        spans.push(Span::styled(prefix.clone(), base));
                    }
                    spans.extend(inline(seg, base));
                }
                out.push(Line::from(spans));
            }
            Kind::Quote => {
                let base = inline_style(Kind::Quote);
                for seg in wrap_text(&row.text, width.saturating_sub(2)) {
                    let mut spans = vec![Span::styled("│ ", base)];
                    spans.extend(inline(&seg, base));
                    out.push(Line::from(spans));
                }
            }
            Kind::List => {
                // 提取前缀符号(如 "-" / "1."),内容从其后开始
                let body = row.text.trim_start();
                let marker_end = body
                    .find(char::is_whitespace)
                    .unwrap_or(body.len());
                let prefix = &body[..marker_end.min(4)];
                let content = body[marker_end..].trim_start();
                let content_w = width.saturating_sub(4).max(8);
                let base = inline_style(Kind::List);
                for (idx, seg) in wrap_text(content, content_w).iter().enumerate() {
                    let mut spans: Vec<Span<'static>> = Vec::new();
                    if idx == 0 {
                        spans.push(Span::styled(format!("  {prefix} "), base));
                    } else {
                        spans.push(Span::styled("      ", base));
                    }
                    spans.extend(inline(seg, base));
                    out.push(Line::from(spans));
                }
            }
            Kind::Normal => {
                let base = inline_style(Kind::Normal);
                for seg in wrap_text(&row.text, width) {
                    out.push(Line::from(inline(&seg, base)));
                }
            }
        }
    }
    // 去掉开头的多余空行
    while out.first().map(|l| l.spans.is_empty()).unwrap_or(false) {
        out.remove(0);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_renders() {
        let lines = render("hello 世界", 30);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn heading_bold() {
        let lines = render("# 标题", 40);
        assert!(!lines.is_empty());
        assert!(lines[0].spans.iter().any(|s| s.content.contains("标题")));
    }

    #[test]
    fn code_block_kept() {
        let md = "```\nlet x = 1;\n```\n后面文字";
        let lines = render(md, 60);
        assert!(lines.iter().any(|l| l.spans.iter().any(|s| s.content.contains("let x"))));
        assert!(lines.iter().any(|l| l.spans.iter().any(|s| s.content.contains("后面"))));
    }

    #[test]
    fn inline_code_and_bold() {
        let lines = render("运行 `cargo build` 用 **双星**", 60);
        assert_eq!(lines.len(), 1);
        let joined: String = lines[0].spans.iter().map(|s| s.content.clone()).collect();
        assert_eq!(joined, "运行 cargo build 用 双星");
    }

    #[test]
    fn wraps_cjk() {
        let lines = render("这是一段非常长的中文内容用来测试换行是否正常工作的文本内容", 10);
        assert!(lines.len() >= 3);
        for l in &lines {
            let w: u16 = l.spans.iter().map(|s| text_width(&s.content)).sum();
            assert!(w <= 12, "行宽 {w} 超限");
        }
    }

    #[test]
    fn wrap_plain_splits_embedded_newlines() {
        let segs = wrap_plain("aaa\nbbb cc", 10);
        assert_eq!(segs.len(), 2, "应含换行分段: {segs:?}");
        assert_eq!(segs[0], "aaa");
        assert_eq!(segs[1], "bbb cc");
        // 超宽内容按新行折行
        let segs2 = wrap_plain("abcdefghijklmnop", 5);
        assert!(segs2.len() >= 3);
        assert!(segs2.iter().all(|s| text_width(s) <= 5));
    }
}
