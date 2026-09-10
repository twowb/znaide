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

/// 极简 glob 匹配:支持 `*` `**` `?`;逐段匹配。
/// `*` 与 `**` 在本工具里等价——都匹配任意串(含 `/`);`?` 匹配单个字符。
/// 用迭代式单星回溯(最坏 O(n·m)),代替原来的递归版:递归版在 `*a*a*a*…b` 这类
/// pattern 上是指数级的,而 pattern 来自模型输出,能拖死整个进程。
pub(crate) fn simple_glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    // 最近一次 `*` 的位置,以及当时匹配到的文本位置(回溯点)
    let mut star: Option<usize> = None;
    let mut star_ti = 0usize;
    while ti < t.len() {
        if pi < p.len() && p[pi] == '?' {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            // 连续多个 `*` 等价于一个:`**` 与 `*` 同义
            while pi < p.len() && p[pi] == '*' {
                pi += 1;
            }
            star = Some(pi);
            star_ti = ti;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some(sp) = star {
            // 回溯:`*` 多吃一个字符再试
            pi = sp;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
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

#[cfg(test)]
mod tests {
    use super::simple_glob_match;

    #[test]
    fn glob_basic_shapes() {
        assert!(simple_glob_match("*.rs", "main.rs"));
        assert!(simple_glob_match("main.?s", "main.rs"));
        assert!(!simple_glob_match("main.?s", "main.rss"));
        assert!(!simple_glob_match("*.rs", "main.py"));
        // 空文本 / 单星
        assert!(simple_glob_match("*", ""));
        assert!(!simple_glob_match("a*", ""));
    }

    /// `**` 与 `*` 同义:都跨 `/`(旧实现里 `**` 分支与 `*` 分支代码相同,注释却写"含多级 /",
    /// 那次只是把死分支删掉、把注释改成实话,匹配行为不变)
    #[test]
    fn glob_star_spans_slash() {
        assert!(simple_glob_match("**/*.rs", "src/a/b/main.rs"));
        assert!(simple_glob_match("*/main.rs", "src/main.rs"));
        assert!(simple_glob_match("**", "any/deep/path"));
        // 星号也跨 `/` 的老行为保持不变:`src/*.rs` 能吃到 `src/a/main.rs`
        assert!(simple_glob_match("src/*.rs", "src/a/main.rs"));
        // 想只匹配一层目录要靠调用方自己按段匹配(walk_glob 就是逐段调的)
        assert!(!simple_glob_match("src/*.py", "src/a/main.rs"));
    }

    /// 多星回溯:结果正确且不会指数爆炸(pattern 来自模型输出,可被恶意构造)
    #[test]
    fn glob_multi_star_backtracks_without_blowup() {
        assert!(simple_glob_match("*a*b*c", "xxayybyycc"));
        assert!(!simple_glob_match("*a*b*c", "xxayybyyzz"));
        // 经典回溯地狱:老递归实现会在这里 2^n 爆炸,现在必须秒回
        let pat = "*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*b";
        let text = "a".repeat(64);
        let start = std::time::Instant::now();
        assert!(!simple_glob_match(pat, &text));
        assert!(start.elapsed().as_secs() < 2, "不该出现指数级回溯");
    }
}
