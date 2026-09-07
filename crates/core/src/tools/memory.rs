//! 长期记忆:跨会话记住用户环境与偏好。
//! 结构:memories/<名称>.md(内容带 frontmatter)+ memories/MEMORY.md(索引)
use super::{ToolContext, ToolError, ToolOutput};
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;

fn memories_dir() -> PathBuf {
    crate::config::data_dir().join("memories")
}

#[derive(Debug, Deserialize)]
struct WriteArgs {
    name: String,
    content: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    mtype: String, // user / project / reference / feedback
}

pub async fn memory_write(ctx: &ToolContext<'_>, args: &Value) -> Result<ToolOutput, ToolError> {
    let _ = ctx;
    let a: WriteArgs = serde_json::from_value(args.clone())?;
    let dir = memories_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| ToolError(format!("创建记忆目录失败: {e}")))?;

    let filename = slugify(&a.name);
    let mtype = if a.mtype.is_empty() {
        "project".to_string()
    } else {
        a.mtype.clone()
    };
    let description = if a.description.is_empty() {
        format!("关于「{}」的记忆", a.name)
    } else {
        a.description.clone()
    };

    let body = format!(
        "---\nname: {}\ndescription: {}\ntype: {}\n---\n\n{}",
        a.name, description, mtype, a.content
    );
    let path = dir.join(format!("{filename}.md"));
    std::fs::write(&path, &body)
        .map_err(|e| ToolError(format!("写入记忆失败: {e}")))?;

    // 更新 MEMORY.md 索引(避免重复条目)
    update_index(&a.name, &description, &filename)?;

    Ok(format!(
        "已保存记忆「{}」(共 {} 字)。索引已更新。",
        a.name,
        a.content.chars().count()
    ))
}

#[derive(Debug, Deserialize)]
struct ReadArgs {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

pub async fn memory_read(ctx: &ToolContext<'_>, args: &Value) -> Result<ToolOutput, ToolError> {
    let _ = ctx;
    let a: ReadArgs = serde_json::from_value(args.clone())?;

    // 按名字精确读
    if let Some(name) = &a.name {
        let path = memories_dir().join(format!("{}.md", slugify(name)));
        if path.exists() {
            let content = std::fs::read_to_string(&path)
                .map_err(|e| ToolError(format!("读取记忆失败: {e}")))?;
            return Ok(content);
        }
        return Err(ToolError(format!("未找到名为「{name}」的记忆")));
    }

    let query = a.query.unwrap_or_default().to_lowercase();
    let mut results: Vec<String> = Vec::new();
    let dir = memories_dir();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        let mut files: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        files.sort();
        for f in files {
            let fname = f.file_name().unwrap_or_default().to_string_lossy().to_string();
            if fname == "MEMORY.md" {
                continue;
            }
            let content = std::fs::read_to_string(&f).unwrap_or_default();
            let lower = content.to_lowercase();
            let matched = query.is_empty() || lower.contains(&query) || fname.to_lowercase().contains(&query);
            if matched {
                // 提取 name / description(frontmatter)
                let (name, desc) = extract_frontmatter(&content);
                let preview: String = content
                    .lines()
                    .filter(|l| !l.starts_with("---"))
                    .skip(1)
                    .take(30)
                    .collect::<Vec<_>>()
                    .join("\n");
                results.push(format!("### {name}\n> {desc}\n```\n{}\n```\n", truncate(&preview, 800)));
            }
        }
    }
    if results.is_empty() {
        return Ok(if query.is_empty() {
            "(记忆库为空)".to_string()
        } else {
            format!("没有找到与「{query}」相关的记忆")
        });
    }
    Ok(format!("找到 {} 条记忆:\n\n{}", results.len(), results.join("\n")))
}

fn update_index(name: &str, description: &str, filename: &str) -> Result<(), ToolError> {
    let index_path = memories_dir().join("MEMORY.md");
    let existing = std::fs::read_to_string(&index_path).unwrap_or_default();
    let line = format!("- [{name}]({filename}.md) — {description}");
    if existing.contains(filename) {
        // 替换该文件对应行
        let lines: Vec<&str> = existing.lines().collect();
        let mut kept: Vec<String> = Vec::new();
        for l in lines {
            if l.contains(filename) {
                kept.push(line.clone());
            } else {
                kept.push(l.to_string());
            }
        }
        std::fs::write(&index_path, kept.join("\n"))
            .map_err(|e| ToolError(format!("更新索引失败: {e}")))?;
    } else {
        let mut content = existing;
        if !content.trim().is_empty() {
            content.push('\n');
        }
        content.push_str(&format!("{line}\n"));
        std::fs::write(&index_path, content)
            .map_err(|e| ToolError(format!("更新索引失败: {e}")))?;
    }
    Ok(())
}

fn extract_frontmatter(content: &str) -> (String, String) {
    let mut name = String::new();
    let mut desc = String::new();
    if content.starts_with("---") {
        for l in content.lines().skip(1) {
            if l.starts_with("---") {
                break;
            }
            if let Some(v) = l.strip_prefix("name:") {
                name = v.trim().to_string();
            }
            if let Some(v) = l.strip_prefix("description:") {
                desc = v.trim().to_string();
            }
        }
    }
    if name.is_empty() {
        name = "(未命名)".into();
    }
    (name, desc)
}

fn slugify(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        }
    }
    if out.is_empty() {
        "memory".into()
    } else {
        out
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_works() {
        assert_eq!(slugify("机器硬件配置"), "机器硬件配置");
        assert_eq!(slugify("My Memory!"), "MyMemory");
    }
}
