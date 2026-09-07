use std::path::{Path, PathBuf};

/// 展开提示词里的 @引用:文件注入内容、目录注入清单;@"带空格路径" 也行。
/// 路径不存在或不可读就保留原文,不吞用户输入。
pub fn expand_at_refs(prompt: &str, cwd: &Path) -> String {
    let chars: Vec<char> = prompt.chars().collect();
    let mut out = String::with_capacity(prompt.len() + 512);
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '@' {
            // @"..." 引号形式:到闭合引号为止(换行视为未闭合)
            if i + 1 < chars.len() && chars[i + 1] == '"' {
                let mut j = i + 2;
                let mut path_str = String::new();
                let mut closed = false;
                while j < chars.len() {
                    let c = chars[j];
                    if c == '"' {
                        closed = true;
                        break;
                    }
                    if c == '\n' || c == '\r' {
                        break;
                    }
                    path_str.push(c);
                    j += 1;
                }
                if closed && !path_str.is_empty() {
                    match expand_one(&path_str, cwd) {
                        Some(text) => {
                            out.push_str(&text);
                            out.push('\n');
                        }
                        None => {
                            out.push('@');
                            out.push('"');
                            out.push_str(&path_str);
                            out.push('"');
                        }
                    }
                    i = j + 1; // 跳过闭合引号
                    continue;
                }
                // 未闭合或空:按原文保留
                out.push('@');
                i += 1;
                continue;
            }
            // 普通路径:收集到空白/标点为止
            let mut j = i + 1;
            let mut path_str = String::new();
            while j < chars.len() {
                let c = chars[j];
                if c.is_whitespace() || matches!(c, ',' | '。' | ';' | '\n' | '\r' | '」' | ')' | ']' | '}' | '：' | ':') {
                    break;
                }
                path_str.push(c);
                j += 1;
            }
            if path_str.is_empty() {
                out.push('@');
                i += 1;
                continue;
            }
            match expand_one(&path_str, cwd) {
                Some(text) => {
                    out.push_str(&text);
                    out.push('\n');
                }
                None => {
                    out.push('@');
                    out.push_str(&path_str);
                }
            }
            i = j;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// 展开单个引用;路径不存在或不可读返回 None(保留原文)
fn expand_one(raw: &str, cwd: &Path) -> Option<String> {
    let p = resolve(raw, cwd);
    if p.is_file() {
        let meta = std::fs::metadata(&p).ok()?;
        if meta.len() > 2_000_000 {
            return Some(format!(
                "[文件 {} 过大({}MB),请用 read_file 工具分页读取]",
                p.display(),
                meta.len() / 1024 / 1024
            ));
        }
        let content = std::fs::read_to_string(&p).ok()?;
        let lines: Vec<&str> = content.lines().collect();
        let shown: Vec<&str> = lines.iter().take(800).copied().collect();
        let mut text = format!("【引用文件 {}】\n```\n", p.display());
        text.push_str(&shown.join("\n"));
        if lines.len() > 800 {
            text.push_str(&format!("\n…(共 {n} 行,仅显示前 800 行)…", n = lines.len()));
        }
        text.push_str("\n```");
        Some(text)
    } else if p.is_dir() {
        let mut entries: Vec<String> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&p) {
            for e in rd.flatten().take(200) {
                let name = e.file_name().to_string_lossy().to_string();
                let suffix = if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    "/"
                } else {
                    ""
                };
                entries.push(format!("{name}{suffix}"));
            }
        }
        entries.sort();
        Some(format!(
            "【引用目录 {} 的内容(前 {} 项)】\n{}",
            p.display(),
            entries.len(),
            entries.join("\n")
        ))
    } else {
        None
    }
}

fn resolve(raw: &str, cwd: &Path) -> PathBuf {
    // 展开开头的 ~
    let s = if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            home.join(rest)
        } else {
            PathBuf::from(raw)
        }
    } else {
        PathBuf::from(raw)
    };
    if s.is_absolute() {
        s
    } else {
        cwd.join(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_file_content() {
        let dir = std::env::temp_dir().join(format!("znaide_refs_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "hello ref").unwrap();
        let out = expand_at_refs("读一下 @a.txt", &dir);
        assert!(out.contains("hello ref"));
        assert!(out.contains("【引用文件"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unknown_ref_kept() {
        let out = expand_at_refs("联系 @notexist123", Path::new("/tmp"));
        assert!(out.contains("@notexist123"));
    }

    #[test]
    fn at_sign_in_email_kept() {
        let out = expand_at_refs("邮件 a@b.com 看看", Path::new("/tmp"));
        assert!(out.contains("a@b.com"));
    }

    #[test]
    fn quoted_ref_with_spaces() {
        // @"带空格路径":含空格的普通 @ 会被截断,引号形式必须可用
        let dir = std::env::temp_dir().join(format!("znaide_refs_q_test_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("my dir")).unwrap();
        std::fs::write(dir.join("my dir/我的 文件.txt"), "含空格内容").unwrap();
        let out = expand_at_refs("读 @\"my dir/我的 文件.txt\" 试试", &dir);
        assert!(out.contains("含空格内容"));
        assert!(out.contains("【引用文件"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn quoted_ref_unknown_kept_verbatim() {
        // 引号引用不存在 → 保留含引号的原文,不吞内容
        let out = expand_at_refs("看 @\"no such 文件.txt\" 好吗", Path::new("/tmp"));
        assert!(out.contains("@\"no such 文件.txt\""));
    }

    #[test]
    fn unclosed_quote_kept() {
        // 未闭合引号(换行前无 "):按原文保留
        let out = expand_at_refs("读 @\"my file\n下一行", Path::new("/tmp"));
        assert!(out.contains("@\"my file"));
        assert!(out.contains("下一行"));
    }
}
