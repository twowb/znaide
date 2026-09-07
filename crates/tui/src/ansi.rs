//! 上屏文本的 ANSI 净化与着色。
//!
//! 终端里显示的不可信文本(模型回复 / 命令输出 / 工具结果)必须先过这里——
//! 真实控制字节被写进终端会被真的执行,画面会错位到只能 resize 救。
//! [`strip`] 直接剥干净;想保留颜色就用 [`render_lines`](SGR → ratatui 样式)。
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// 显示宽度近似(与 md.rs 的 `wc` 保持一致:CJK/全角/常见 emoji 按 2 列,
/// 控制字符 0 列)。两边要一起改。
fn wc(c: char) -> u16 {
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

/// 剥掉全部 ESC 转义与 C0 控制字符;保留 `\n` `\t`。
/// `\r` 直接丢(CRLF 靠 `\n` 换行;孤立的 \r 在静态显示里没意义);
/// 未终结的残尾一并吞掉,不留可执行字节。
pub fn strip(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => skip_escape(&mut chars),
            '\n' | '\t' => out.push(c),
            '\r' => {}
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// 消费一个 ESC 引导的转义序列(调用前已消费掉 `\x1b`)。
fn skip_escape(chars: &mut std::str::Chars<'_>) {
    let Some(c) = chars.next() else { return };
    match c {
        '[' => {
            // CSI:参数与中间字节之后是终结字节(0x40..=0x7e);非 SGR 的序列一律丢弃
            for c in chars.by_ref() {
                let code = c as u32;
                if (0x40..=0x7e).contains(&code) {
                    break;
                }
            }
        }
        ']' => {
            // OSC:读到 BEL 或 ESC \ 结束
            let mut prev_esc = false;
            for c in chars.by_ref() {
                if c == '\x07' {
                    break;
                }
                if prev_esc {
                    if c == '\\' {
                        break;
                    }
                    prev_esc = false;
                } else if c == '\x1b' {
                    prev_esc = true;
                }
            }
        }
        _ => {
            // 两字符序列(ESC X):剥掉 X。若 X 恰好又是 ESC,一并丢弃。
        }
    }
}

/// SGR 参数 → ratatui 样式(仅 [`render_lines`] 用)。
/// 支持:0/1/3/4/7 及取消、30-37/90-97 前景、40-47/100-107 背景、39/49 还原、
/// 38/48 的 `;5;n`(256 色)与 `;2;r;g;b`(真彩)。
fn apply_sgr(style: &mut Style, base: Style, raw: &str) {
    const BASIC: [Color; 8] = [
        Color::Black,
        Color::Red,
        Color::Green,
        Color::Yellow,
        Color::Blue,
        Color::Magenta,
        Color::Cyan,
        Color::Gray,
    ];
    let params: Vec<u16> = raw.split(';').filter_map(|p| p.parse().ok()).collect();
    if params.is_empty() {
        *style = base; // 空参数 `ESC[m` 等价 0 = 重置
        return;
    }
    let mut i = 0;
    while i < params.len() {
        let p = params[i];
        match p {
            0 => *style = base,
            1 => *style = style.add_modifier(Modifier::BOLD),
            3 => *style = style.add_modifier(Modifier::ITALIC),
            4 => *style = style.add_modifier(Modifier::UNDERLINED),
            7 => *style = style.add_modifier(Modifier::REVERSED),
            22 => *style = style.remove_modifier(Modifier::BOLD),
            23 => *style = style.remove_modifier(Modifier::ITALIC),
            24 => *style = style.remove_modifier(Modifier::UNDERLINED),
            27 => *style = style.remove_modifier(Modifier::REVERSED),
            30..=37 => style.fg = Some(BASIC[(p - 30) as usize]),
            39 => style.fg = None,
            40..=47 => style.bg = Some(BASIC[(p - 40) as usize]),
            49 => style.bg = None,
            // 亮色(90-97 / 100-107)近似为基础色(ratatui 无亮色调色板)
            90..=97 => style.fg = Some(BASIC[(p - 90) as usize]),
            100..=107 => style.bg = Some(BASIC[(p - 100) as usize]),
            38 | 48 => {
                let is_fg = p == 38;
                i += 1;
                if i < params.len() {
                    match params[i] {
                        5 if i + 1 < params.len() => {
                            let c = Color::Indexed(params[i + 1] as u8);
                            if is_fg {
                                style.fg = Some(c);
                            } else {
                                style.bg = Some(c);
                            }
                            i += 1;
                        }
                        2 if i + 3 < params.len() => {
                            let c = Color::Rgb(params[i + 1] as u8, params[i + 2] as u8, params[i + 3] as u8);
                            if is_fg {
                                style.fg = Some(c);
                            } else {
                                style.bg = Some(c);
                            }
                            i += 3;
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
}

/// 一段文本 + 该段样式(一个逻辑行内若干段拼接而成)。
type Seg = (String, Style);

/// 解析一段可能带 ANSI 的文本为"逻辑行 × (文本,样式)"。
/// 过程中剥掉控制字节、SGR 转样式;空逻辑行保留以维持原换行。
fn parse_rows(text: &str, base: Style) -> Vec<Vec<Seg>> {
    let mut rows: Vec<Vec<Seg>> = vec![Vec::new()];
    let mut seg = String::new();
    let mut style = base;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {
                // 样式可能变化:先结算当前段,再吞序列、更新样式
                if !seg.is_empty() {
                    rows.last_mut().unwrap().push((std::mem::take(&mut seg), style));
                }
                let mut save = style;
                consume_sgr(&mut chars, &mut save, base);
                style = save;
            }
            '\n' => {
                if !seg.is_empty() {
                    rows.last_mut().unwrap().push((std::mem::take(&mut seg), style));
                }
                rows.push(Vec::new());
            }
            '\r' => {}
            c if c.is_control() && c != '\t' => {}
            _ => seg.push(c),
        }
    }
    if !seg.is_empty() {
        rows.last_mut().unwrap().push((seg, style));
    }
    rows
}

/// 同 [`skip_escape`],但若是 SGR(`ESC[...m`)则把参数应用到样式上。
fn consume_sgr(chars: &mut std::str::Chars<'_>, style: &mut Style, base: Style) {
    let Some(c) = chars.next() else { return };
    if c != '[' {
        return; // 非 CSI 的转义:剥掉即可(样式不变)
    }
    let mut params = String::new();
    for c in chars.by_ref() {
        let code = c as u32;
        if (0x40..=0x7e).contains(&code) {
            if c == 'm' {
                apply_sgr(style, base, &params);
            }
            return;
        }
        if c.is_ascii_digit() || c == ';' || c == ':' {
            params.push(c);
        }
    }
}

/// 把一个逻辑行(若干段)按显示宽度折成若干物理行。
/// 行内断点跨段时按字符精确切分;单字符宽度超过 width 时单独成行(由外层裁切)。
fn wrap_row(segs: Vec<Seg>, width: u16) -> Vec<Line<'static>> {
    if width <= 2 || segs.is_empty() {
        // 超窄区:不折行;空段产生一个空行占位
        if segs.is_empty() {
            return vec![Line::from("")];
        }
        return vec![Line::from(
            segs.into_iter()
                .map(|(t, _s)| t)
                .collect::<String>(),
        )];
    }
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0u16;
    for (text, style) in segs {
        for ch in text.chars() {
            let w = wc(ch);
            if w > 0 && cur_w + w > width && !spans.is_empty() {
                out.push(Line::from(std::mem::take(&mut spans)));
                cur_w = 0;
            }
            cur_w += w;
            // 与上一个 span 样式相同则续写,避免碎片化
            if let Some(last) = spans.last_mut() {
                if last.style == style {
                    last.content.to_mut().push(ch);
                    continue;
                }
            }
            let mut s = String::new();
            s.push(ch);
            spans.push(Span::styled(s, style));
        }
    }
    if !spans.is_empty() {
        out.push(Line::from(spans));
    } else if out.is_empty() {
        out.push(Line::from(""));
    }
    out
}

/// 把一段可能含 ANSI 的文本渲染成按 `width` 折行、可上屏的行。
/// SGR 颜色解析为样式叠加在 `base` 之上;非 SGR 转义与控制字符一律剥除。
/// 主要用于命令/工具输出等"机器文本"(不跑 markdown)。
pub fn render_lines(text: &str, width: u16, base: Style) -> Vec<Line<'static>> {
    let rows = parse_rows(text, base);
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.extend(wrap_row(row, width));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Style;

    #[test]
    fn strip_removes_sgr_sequences() {
        assert_eq!(strip("a\x1b[31mb\x1b[0mc"), "abc");
        assert_eq!(strip("\x1b[2J"), "");
        assert_eq!(strip("\x1b]0;title\x07rest"), "rest");
    }

    #[test]
    fn strip_handles_cr_and_c0() {
        // CRLF → \n;孤立 \r 丢弃
        assert_eq!(strip("a\r\nb"), "a\nb");
        assert_eq!(strip("a\rb"), "ab");
        // 其它 C0(如 \x07 铃声)剥;保留 \n \t
        assert_eq!(strip("a\x07b\x01c\n\td"), "abc\n\td");
    }

    #[test]
    fn strip_keeps_utf8_and_literal_backslash() {
        // 字面 "\x1b"(反斜杠+x1b)不是真实 ESC,原样保留
        assert_eq!(strip("中文\\x1b[31m ok"), "中文\\x1b[31m ok");
        assert_eq!(strip("中文真实\x1b[31m色"), "中文真实色");
    }

    #[test]
    fn strip_tolerates_unterminated_escape() {
        assert_eq!(strip("abc\x1b[31"), "abc");
        assert_eq!(strip("abc\x1b"), "abc");
    }

    #[test]
    fn render_colors_sgr_to_style() {
        let base = Style::default();
        let lines = render_lines("\x1b[31mred\x1b[0m plain", 80, base);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans.len(), 2);
        assert_eq!(lines[0].spans[0].content, "red");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Red));
        assert_eq!(lines[0].spans[1].content, " plain");
        assert_eq!(lines[0].spans[1].style.fg, None);
    }

    #[test]
    fn render_extends_base_style_and_strips_controls() {
        let base = Style::default().fg(Color::DarkGray);
        let lines = render_lines("\x1b[1m\x1b[32mbold green\x1b[0m\x1b[2J tail", 80, base);
        assert_eq!(lines.len(), 1);
        let segs = &lines[0].spans;
        assert!(segs.iter().any(|s| s.style.fg == Some(Color::Green)));
        assert!(segs.iter().any(|s| s.style.fg == Some(Color::DarkGray)));
        let joined: String = segs.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "bold green tail");
    }

    #[test]
    fn render_wraps_by_display_width() {
        let lines = render_lines("一二三四五六七八九十", 6, Style::default());
        // 6 列 → 每行 3 个汉字
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0].spans[0].content, "一二三");
    }

    #[test]
    fn render_keeps_blank_lines() {
        let lines = render_lines("a\n\nb", 80, Style::default());
        assert_eq!(lines.len(), 3);
        assert!(lines[1].spans.is_empty() || lines[1].spans[0].content.is_empty());
    }
}
