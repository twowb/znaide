use super::{resolve_path, ToolContext, ToolError, ToolOutput};
use serde::Deserialize;
use serde_json::Value;
use std::path::Path;

#[derive(Debug, Deserialize)]
struct ListArgs {
    #[serde(default)]
    path: Option<String>,
}

pub async fn list_directory(ctx: &ToolContext<'_>, args: &Value) -> Result<ToolOutput, ToolError> {
    let a: ListArgs = serde_json::from_value(args.clone())?;
    // path 可选:缺省列当前工作目录(模型常不带参数调用,不再报错)
    let path = match &a.path {
        Some(p) => resolve_path(ctx.cwd, p),
        None => ctx.cwd.to_path_buf(),
    };
    let mut entries = Vec::new();
    let mut rd = tokio::fs::read_dir(&path)
        .await
        .map_err(|e| ToolError(format!("无法读取目录 {}: {e}", path.display())))?;
    while let Some(entry) = rd.next_entry().await? {
        let ft = entry.file_type().await?;
        let name = entry.file_name().to_string_lossy().to_string();
        if ft.is_dir() {
            entries.push(format!("{name}/"));
        } else {
            entries.push(name);
        }
    }
    entries.sort();
    let dir = if entries.is_empty() {
        "(空目录)".to_string()
    } else {
        entries.join("\n")
    };
    Ok(format!("目录 {}:\n{dir}", path.display()))
}

#[derive(Debug, Deserialize)]
struct GlobArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
}

pub async fn glob(ctx: &ToolContext<'_>, args: &Value) -> Result<ToolOutput, ToolError> {
    let a: GlobArgs = serde_json::from_value(args.clone())?;
    let root = match &a.path {
        Some(p) => resolve_path(ctx.cwd, p),
        None => ctx.cwd.to_path_buf(),
    };
    let pattern = if a.pattern.contains('/') {
        a.pattern.clone()
    } else {
        format!("**/{}", a.pattern)
    };
    let mut matches = walk_glob(&root, &pattern)?;
    matches.sort();
    if matches.is_empty() {
        return Ok(format!("在 {} 下没有匹配 {pattern} 的文件", root.display()));
    }
    let shown: Vec<String> = matches
        .iter()
        .map(|m| m.display().to_string())
        .collect();
    Ok(format!("匹配 {} 个文件:\n{}", shown.len(), shown.join("\n")))
}

/// 简化 glob:遍历目录树,用通配匹配相对路径段
fn walk_glob(root: &Path, pattern: &str) -> Result<Vec<std::path::PathBuf>, ToolError> {
    let pat = pattern.trim_start_matches("./");
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for entry in rd.flatten() {
            let p = entry.path();
            let rel = match p.strip_prefix(root) {
                Ok(r) => r.to_string_lossy().replace('\\', "/"),
                Err(_) => continue,
            };
            if simple_glob_match(pat, &rel) {
                out.push(p.clone());
            }
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                stack.push(p);
            }
        }
    }
    Ok(out)
}

/// 极简 glob 匹配:支持 * ** ? ;逐段匹配
pub(crate) fn simple_glob_match(pattern: &str, text: &str) -> bool {
    // ** 直接匹配剩余
    if pattern == "**" {
        return true;
    }
    let pat_chars: Vec<char> = pattern.chars().collect();
    let txt_chars: Vec<char> = text.chars().collect();
    // 递归式匹配
    fn m(p: &[char], t: &[char]) -> bool {
        if p.is_empty() {
            return t.is_empty();
        }
        match p[0] {
            '*' => {
                if p.len() > 1 && p[1] == '*' {
                    // ** 匹配任意(含多级 /) —— 简化:与 * 等价后继续匹配
                    return m(&p[1..], t)
                        || (!t.is_empty() && m(p, &t[1..]));
                }
                m(&p[1..], t) || (!t.is_empty() && m(p, &t[1..]))
            }
            '?' => !t.is_empty() && m(&p[1..], &t[1..]),
            c => !t.is_empty() && t[0] == c && m(&p[1..], &t[1..]),
        }
    }
    m(&pat_chars, &txt_chars)
}

#[derive(Debug, Deserialize)]
struct GrepArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    glob: Option<String>,
}

pub async fn grep_search(ctx: &ToolContext<'_>, args: &Value) -> Result<ToolOutput, ToolError> {
    let a: GrepArgs = serde_json::from_value(args.clone())?;
    let target = match &a.path {
        Some(p) => resolve_path(ctx.cwd, p),
        None => ctx.cwd.to_path_buf(),
    };
    let re = regex::Regex::new(&a.pattern)
        .map_err(|e| ToolError(format!("正则表达式无效: {e}")))?;

    let mut files: Vec<std::path::PathBuf> = Vec::new();
    if target.is_file() {
        files.push(target);
    } else {
        let mut stack = vec![target];
        while let Some(dir) = stack.pop() {
            let rd = match std::fs::read_dir(&dir) {
                Ok(r) => r,
                Err(_) => continue,
            };
            for entry in rd.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    // 跳过常见噪音目录
                    let name = entry.file_name().to_string_lossy().to_string();
                    if !matches!(name.as_str(), "target" | ".git" | "node_modules" | ".qwen") {
                        stack.push(p);
                    }
                } else {
                    files.push(p);
                }
            }
        }
    }

    let mut results = Vec::new();
    for f in files {
        if let Some(g) = &a.glob {
            let rel = f.to_string_lossy().replace('\\', "/");
            if !simple_glob_match(g, &rel) && !f.ends_with(g) {
                continue;
            }
        }
        let content = match std::fs::read_to_string(&f) {
            Ok(c) => c,
            Err(_) => continue, // 二进制/不可读跳过
        };
        for (i, line) in content.lines().enumerate() {
            if re.is_match(line) {
                results.push(format!("{}:{}:{}", f.display(), i + 1, line));
            }
        }
    }
    if results.is_empty() {
        return Ok("没有匹配结果".to_string());
    }
    let shown = results.iter().take(200).cloned().collect::<Vec<_>>();
    let more = if results.len() > 200 {
        format!("\n…(还有 {} 条未显示)", results.len() - 200)
    } else {
        String::new()
    };
    Ok(format!("找到 {} 条匹配:\n{}{}", results.len(), shown.join("\n"), more))
}
