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
    let (name, desc, _mtype) = extract_meta(content);
    (name, desc)
}

/// 解析 frontmatter 的 name / description / type(缺省补齐)。
/// 注意:这里**故意**没和 skills::parse_frontmatter 合并——两者语义不同:
/// 记忆文件的值不做去引号、name 缺省补 "(未命名)",还多认一个 `type:` 字段;
/// 硬合并会改掉记忆的既有解析结果。真要统一时得先确认这两种读法的取舍。
fn extract_meta(content: &str) -> (String, String, String) {
    let mut name = String::new();
    let mut desc = String::new();
    let mut mtype = String::new();
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
            if let Some(v) = l.strip_prefix("type:") {
                mtype = v.trim().to_string();
            }
        }
    }
    if name.is_empty() {
        name = "(未命名)".into();
    }
    (name, desc, mtype)
}

/// 一条记忆的目录项(供会话管理窗口的记忆页展示/删除)
#[derive(Debug, Clone)]
pub struct MemoryEntry {
    /// frontmatter 里的名字(展示用)
    pub name: String,
    /// 文件名主段(无 .md;删除时传这个,已 slug 化、无路径成分)
    pub filename: String,
    pub description: String,
    /// user / project / reference / feedback(空 = 未知)
    pub mtype: String,
    pub size: u64,
    /// 修改时间(unix 秒)
    pub modified: u64,
}

/// 列出全部记忆条目(不含 MEMORY.md 索引),按修改时间新→旧。
/// 用于用户管理界面(/resume 管理窗口记忆页);目录不存在返回空。
pub fn list_memories() -> Vec<MemoryEntry> {
    use std::time::UNIX_EPOCH;
    let dir = memories_dir();
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            let fname = p.file_name().unwrap_or_default().to_string_lossy().to_string();
            if !fname.ends_with(".md") || fname == "MEMORY.md" {
                continue;
            }
            let content = std::fs::read_to_string(&p).unwrap_or_default();
            let (name, desc, mtype) = extract_meta(&content);
            let meta = e.metadata().ok();
            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let modified = meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            out.push(MemoryEntry {
                name,
                filename: fname.trim_end_matches(".md").to_string(),
                description: desc,
                mtype,
                size,
                modified,
            });
        }
    }
    out.sort_by_key(|m| m.modified);
    out.reverse();
    out
}

/// 删除一条记忆:删 <filename>.md 并同步清掉 MEMORY.md 索引里对应行。
/// filename 为无 .md 的文件主段(来自 list_memories);找不到返回 Ok(false)。
pub fn delete_memory(filename: &str) -> anyhow::Result<bool> {
    let safe = slugify(filename);
    if safe.is_empty() {
        return Ok(false);
    }
    let path = memories_dir().join(format!("{safe}.md"));
    if !path.exists() {
        return Ok(false);
    }
    std::fs::remove_file(&path)
        .map_err(|e| anyhow::anyhow!("删除记忆文件失败: {e}"))?;
    // 同步清索引行(幂等:行不存在也照常写回)
    let index_path = memories_dir().join("MEMORY.md");
    if index_path.exists() {
        let existing = std::fs::read_to_string(&index_path).unwrap_or_default();
        let kept: Vec<&str> = existing.lines().filter(|l| !l.contains(&safe)).collect();
        let mut cleaned = kept.join("\n");
        if !cleaned.is_empty() {
            cleaned.push('\n');
        }
        if cleaned != existing {
            std::fs::write(&index_path, cleaned)
                .map_err(|e| anyhow::anyhow!("更新记忆索引失败: {e}"))?;
        }
    }
    Ok(true)
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
    crate::util::truncate_chars(s, max, "…")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 先锁后设独立数据目录(与 undo/session 测试同一把锁,避免 env 竞态)。
    /// tag 区分各测试,避免共享目录里文件互相污染断言。
    fn isolate(tag: &str) -> std::sync::MutexGuard<'static, ()> {
        let g = crate::test_util::DATA_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("znaide_mem_ut_{}_{tag}", std::process::id()));
        std::env::set_var("ZNAIDE_DATA_DIR", &dir);
        std::fs::create_dir_all(memories_dir()).unwrap();
        g
    }

    fn write_mem(filename: &str, body: &str) {
        std::fs::write(memories_dir().join(format!("{filename}.md")), body).unwrap();
    }

    #[test]
    fn slug_works() {
        assert_eq!(slugify("机器硬件配置"), "机器硬件配置");
        assert_eq!(slugify("My Memory!"), "MyMemory");
        assert_eq!(slugify(""), "memory");
    }

    /// 列表:解析 frontmatter、跳过索引文件、字段齐全
    #[test]
    fn list_parses_entries_and_skips_index() {
        let _g = isolate("list");
        write_mem(
            "machine",
            "---\nname: 机器硬件配置\ndescription: GPU 型号等\ntype: user\n---\n\nRTX 4090",
        );
        write_mem("plain", "没有 frontmatter 的旧格式");
        write_mem("MEMORY", "# 索引\n- [机器硬件配置](machine.md) — GPU 型号等");

        let list = list_memories();
        assert_eq!(list.len(), 2, "索引文件不应计入");
        let machine = list.iter().find(|m| m.filename == "machine").unwrap();
        assert_eq!(machine.name, "机器硬件配置");
        assert_eq!(machine.description, "GPU 型号等");
        assert_eq!(machine.mtype, "user");
        assert!(machine.size > 0);
        let plain = list.iter().find(|m| m.filename == "plain").unwrap();
        assert_eq!(plain.name, "(未命名)", "无 frontmatter 应兜底");
    }

    /// 删除:文件与索引行同步消失;索引里其它条目保留;找不到幂等返回 false
    #[test]
    fn delete_removes_file_and_index_line() {
        let _g = isolate("del");
        write_mem("keep", "---\nname: 保留\ntype: project\n---\nx");
        write_mem("drop", "---\nname: 删除我\ntype: user\n---\ny");
        std::fs::write(
            memories_dir().join("MEMORY.md"),
            "- [保留](keep.md) — 1\n- [删除我](drop.md) — 2\n",
        )
        .unwrap();

        assert!(delete_memory("drop").unwrap(), "应删到");
        assert!(!memories_dir().join("drop.md").exists(), "文件应删");
        let idx = std::fs::read_to_string(memories_dir().join("MEMORY.md")).unwrap();
        assert!(!idx.contains("drop"), "索引行应清掉");
        assert!(idx.contains("keep"), "其它条目保留");
        assert!(!delete_memory("drop").unwrap(), "再删应返回 false");
        // 目录/文件不存在时列表为空、删除不崩
        let empty_dir = std::env::temp_dir().join(format!("znaide_mem_none_{}", std::process::id()));
        std::env::set_var("ZNAIDE_DATA_DIR", &empty_dir);
        assert!(list_memories().is_empty());
        assert!(!delete_memory("ghost").unwrap());
    }
}
