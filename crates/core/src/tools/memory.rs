//! 长期记忆:跨会话记住用户环境与偏好。
//!
//! **两级**(各自独立目录与 MEMORY.md 索引):
//! - Global:`~/.znaide/memories/`(随用户走;`ZNAIDE_DATA_DIR` 可覆盖)—— 个人偏好、机器环境
//! - Project:`<cwd>/.znaide/memories/`(随仓库走,可提交可 ignore)—— 本仓库的约定/架构/待办
//!
//! 结构:memories/<名称>.md(内容带 frontmatter)+ memories/MEMORY.md(索引)。
//! 两级的名字可以相同(互不覆盖):读的时候都返回并标来源,删的时候必须指明作用域。
use super::{ToolContext, ToolError, ToolOutput};
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// 记忆的作用域(存放位置)。与 frontmatter 里的 `mtype`(分类标签)是**两回事**:
/// `mtype` 想说"这是什么类型的记忆",`scope` 说"它存在哪里"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// 全局:随用户走
    Global,
    /// 项目级:随当前工作目录走
    Project,
}

impl Scope {
    /// 认不出的返回 None(调用方决定是报错还是回落)
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "global" | "user" => Some(Self::Global),
            "project" | "repo" => Some(Self::Project),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project => "project",
        }
    }

    /// 中文标签(展示/文案用)
    pub fn label(&self) -> &'static str {
        match self {
            Self::Global => "全局",
            Self::Project => "项目",
        }
    }
}

/// 全局记忆目录(数据目录下,`ensure_data_dirs` 已建)
fn global_dir() -> PathBuf {
    crate::config::data_dir().join("memories")
}

/// 项目级记忆目录(跟着 cwd 走)。**不主动创建**:目录不存在就当作"这个项目还没有记忆",
/// 免得每进一个目录都在仓库里留个空目录;第一次写 project 记忆时才建。
fn project_dir(cwd: &Path) -> PathBuf {
    cwd.join(".znaide").join("memories")
}

fn dir_for(scope: Scope, cwd: &Path) -> PathBuf {
    match scope {
        Scope::Global => global_dir(),
        Scope::Project => project_dir(cwd),
    }
}

/// 拼出作用域目录内的记忆文件路径(文件名主段 → <dir>/<stem>.md)。
///
/// 两道保险:`slugify` 现在只放行字母数字/`-`/`_`,本来吐不出路径成分;这里再确认
/// 一次最终路径的父目录就是作用域目录 —— 万一以后有人放宽 slugify(`..` 或 `/`
/// 漏进来),写/删就会跑出作用域目录,那是最难查的一类事故。
fn scoped_path(dir: &Path, stem: &str) -> Option<PathBuf> {
    let path = dir.join(format!("{stem}.md"));
    (path.parent() == Some(dir)).then_some(path)
}

#[derive(Debug, Deserialize)]
struct WriteArgs {
    name: String,
    content: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    mtype: String, // user / project / reference / feedback
    #[serde(default)]
    scope: String, // global(缺省)/ project
}

/// 解析写入的作用域:缺省 global(最保守 —— 老调用行为不变,也不会顺手往仓库里写)。
fn write_scope(raw: &str) -> Result<Scope, ToolError> {
    match raw.trim() {
        "" => Ok(Scope::Global),
        s => Scope::parse(s).ok_or_else(|| {
            ToolError(format!(
                "scope 只能是 global 或 project(收到「{s}」)。个人偏好/机器环境用 global,本仓库的约定用 project"
            ))
        }),
    }
}

pub async fn memory_write(ctx: &ToolContext<'_>, args: &Value) -> Result<ToolOutput, ToolError> {
    let a: WriteArgs = serde_json::from_value(args.clone())?;
    let scope = write_scope(&a.scope)?;
    let dir = dir_for(scope, ctx.cwd);
    std::fs::create_dir_all(&dir)
        .map_err(|e| ToolError(format!("创建记忆目录失败: {e}")))?;

    let filename = slugify(&a.name);
    let path = scoped_path(&dir, &filename)
        .ok_or_else(|| ToolError(format!("记忆名不合法:{filename}")))?;
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
    std::fs::write(&path, &body)
        .map_err(|e| ToolError(format!("写入记忆失败: {e}")))?;

    // 更新该作用域自己的 MEMORY.md 索引(避免重复条目)
    update_index(&dir, &a.name, &description, &filename)?;

    Ok(format!(
        "已保存{}记忆「{}」(共 {} 字)。{}索引已更新。",
        scope.label(),
        a.name,
        a.content.chars().count(),
        if scope == Scope::Project {
            format!("位置:{}\n", dir.display())
        } else {
            String::new()
        }
    ))
}

#[derive(Debug, Deserialize)]
struct ReadArgs {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    scope: String, // 空 / all = 两级都读
}

pub async fn memory_read(ctx: &ToolContext<'_>, args: &Value) -> Result<ToolOutput, ToolError> {
    let a: ReadArgs = serde_json::from_value(args.clone())?;
    let cwd = ctx.cwd;
    // 缺省 = 两级都读;显式给一级就只读那一级
    let only: Option<Scope> = match a.scope.trim().to_ascii_lowercase().as_str() {
        "" | "all" | "auto" => None,
        s => Some(Scope::parse(s).ok_or_else(|| {
            ToolError(format!("scope 只能是 global / project / all(收到「{s}」)"))
        })?),
    };
    let scopes: Vec<Scope> = match only {
        Some(s) => vec![s],
        // 项目在前:同名的两条都返回,但先说"这个项目的"
        None => vec![Scope::Project, Scope::Global],
    };

    // 按名字精确读
    if let Some(name) = &a.name {
        let stem = slugify(name);
        let mut hits: Vec<(Scope, String)> = Vec::new();
        for scope in scopes {
            let dir = dir_for(scope, cwd);
            let Some(path) = scoped_path(&dir, &stem) else {
                continue;
            };
            if let Ok(content) = std::fs::read_to_string(&path) {
                hits.push((scope, content));
            }
        }
        return match hits.len() {
            0 => Err(ToolError(format!("未找到名为「{name}」的记忆"))),
            // 只有一份:照旧直接回正文,不额外加包装
            1 => Ok(hits.remove(0).1),
            // 两级同名:都回,并标清哪份是哪级(别静默只给一条)
            _ => Ok(hits
                .into_iter()
                .map(|(s, c)| format!("### {name}({}记忆)\n{c}", s.label()))
                .collect::<Vec<_>>()
                .join("\n\n")),
        };
    }

    let query = a.query.unwrap_or_default().to_lowercase();
    let mut sections: Vec<String> = Vec::new();
    let mut found = 0usize;
    for scope in scopes {
        let hits = search_in(&dir_for(scope, cwd), &query);
        if hits.is_empty() {
            continue;
        }
        found += hits.len();
        // 只读一级时不加分组标题;两级都读时标出来源
        if only.is_some() {
            sections.push(hits.join("\n"));
        } else {
            sections.push(format!("## {}记忆\n{}", scope.label(), hits.join("\n")));
        }
    }
    if found == 0 {
        return Ok(if query.is_empty() {
            "(记忆库为空)".to_string()
        } else {
            format!("没有找到与「{query}」相关的记忆")
        });
    }
    Ok(format!("找到 {found} 条记忆:\n\n{}", sections.join("\n\n")))
}

/// 在一个记忆目录里按关键词(空 = 全部)搜索,返回渲染好的条目块
fn search_in(dir: &Path, query: &str) -> Vec<String> {
    let mut results: Vec<String> = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return results;
    };
    let mut files: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    files.sort();
    for f in files {
        let fname = f.file_name().unwrap_or_default().to_string_lossy().to_string();
        if fname == "MEMORY.md" {
            continue;
        }
        let content = std::fs::read_to_string(&f).unwrap_or_default();
        let lower = content.to_lowercase();
        let matched =
            query.is_empty() || lower.contains(query) || fname.to_lowercase().contains(query);
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
            results.push(format!(
                "### {name}\n> {desc}\n```\n{}\n```\n",
                truncate(&preview, 800)
            ));
        }
    }
    results
}

fn update_index(
    dir: &Path,
    name: &str,
    description: &str,
    filename: &str,
) -> Result<(), ToolError> {
    let index_path = dir.join("MEMORY.md");
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

/// 该条记忆在磁盘上的路径(界面展示正文用)。**作用域必须一起给** ——
/// 两级可能有同名文件,只凭文件名会读到另一级那份。
pub fn memory_path(filename: &str, scope: Scope, cwd: &Path) -> Option<PathBuf> {
    scoped_path(&dir_for(scope, cwd), &slugify(filename))
}

/// 一条记忆的目录项(供会话管理窗口的记忆页展示/删除)
#[derive(Debug, Clone)]
pub struct MemoryEntry {
    /// frontmatter 里的名字(展示用)
    pub name: String,
    /// 文件名主段(无 .md;删除时传这个,已 slug 化、无路径成分)
    pub filename: String,
    pub description: String,
    /// user / project / reference / feedback(空 = 未知)。**分类标签**,
    /// 与 `source`(存在哪一级)是两回事
    pub mtype: String,
    /// 存在哪一级(Global / Project)
    pub source: Scope,
    pub size: u64,
    /// 修改时间(unix 秒)
    pub modified: u64,
}

/// 列出**两级**全部记忆条目(不含 MEMORY.md 索引)。
/// 顺序:项目级在前、全局在后,各自内部按修改时间新→旧(与 /resume 记忆页的
/// "先看本项目的"预期一致)。目录不存在视为空。
pub fn list_memories(cwd: &Path) -> Vec<MemoryEntry> {
    let mut out = Vec::new();
    for scope in [Scope::Project, Scope::Global] {
        let mut rows = list_in(&dir_for(scope, cwd), scope);
        rows.sort_by_key(|m| m.modified);
        rows.reverse();
        out.extend(rows);
    }
    out
}

fn list_in(dir: &Path, source: Scope) -> Vec<MemoryEntry> {
    use std::time::UNIX_EPOCH;
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
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
                source,
                size,
                modified,
            });
        }
    }
    out
}

/// 删除一条记忆:删 <scope 目录>/<filename>.md 并同步清掉该级 MEMORY.md 索引里对应行。
/// filename 为无 .md 的文件主段(来自 list_memories);找不到返回 Ok(false)。
///
/// **scope 必传**:两级可能有同名记忆,只给文件名没法知道删哪个 —— 传错级等于
/// 删掉了另一个项目的记录,这种事故没法从"删了 1 条"的返回里看出来。
pub fn delete_memory(filename: &str, scope: Scope, cwd: &Path) -> anyhow::Result<bool> {
    let safe = slugify(filename);
    if safe.is_empty() {
        return Ok(false);
    }
    let dir = dir_for(scope, cwd);
    let Some(path) = scoped_path(&dir, &safe) else {
        anyhow::bail!("记忆名不合法:{filename}");
    };
    if !path.exists() {
        return Ok(false);
    }
    std::fs::remove_file(&path)
        .map_err(|e| anyhow::anyhow!("删除记忆文件失败: {e}"))?;
    // 同步清索引行(幂等:行不存在也照常写回)
    let index_path = dir.join("MEMORY.md");
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
    use crate::permissions::{Mode, Permission};
    use serde_json::json;

    /// 先锁后设独立数据目录(与 undo/session 测试同一把锁,避免 env 竞态),
    /// 同时给一个临时 cwd 当"项目目录"。返回 (锁, cwd)。
    fn isolate(tag: &str) -> (std::sync::MutexGuard<'static, ()>, PathBuf) {
        let g = crate::test_util::DATA_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let base = std::env::temp_dir().join(format!("znaide_mem_ut_{}_{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let data = base.join("data");
        let cwd = base.join("cwd");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::env::set_var("ZNAIDE_DATA_DIR", &data);
        std::fs::create_dir_all(global_dir()).unwrap();
        (g, cwd)
    }

    fn write_mem(dir: impl AsRef<Path>, filename: &str, body: &str) {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(format!("{filename}.md")), body).unwrap();
    }

    /// 造一个最小 ToolContext(memory 工具只用到 cwd)
    fn ctx<'a>(cwd: &'a Path, perm: &'a Permission) -> ToolContext<'a> {
        ToolContext {
            cwd,
            permission: perm,
            session_id: "mem-test",
            cancel: None,
            events: None,
            proxy: &crate::config::EffectiveProxy::Direct,
        }
    }

    #[test]
    fn slug_works() {
        assert_eq!(slugify("机器硬件配置"), "机器硬件配置");
        assert_eq!(slugify("My Memory!"), "MyMemory");
        assert_eq!(slugify(""), "memory");
        // 路径成分进不来(第一道保险)
        assert_eq!(slugify("../etc/passwd"), "etcpasswd");
        // 第二道保险:拼出的路径必须还在作用域目录里
        let dir = Path::new("/a/b");
        assert!(scoped_path(dir, "ok").is_some());
        assert!(scoped_path(dir, "../x").is_none());
    }

    /// 写:缺省落全局;scope=project 落 cwd,且全局那份不受影响
    #[test]
    fn write_defaults_to_global_project_goes_to_cwd() {
        let (_g, cwd) = isolate("w");
        let perm = Permission::new(Mode::BypassPermissions);
        let c = ctx(&cwd, &perm);
        let rt = tokio::runtime::Runtime::new().unwrap();

        let out = rt
            .block_on(memory_write(
                &c,
                &json!({"name": "偏好", "content": "喜欢简洁"}),
            ))
            .unwrap();
        assert!(out.contains("全局"), "{out}");
        assert!(global_dir().join("偏好.md").exists());
        assert!(!project_dir(&cwd).exists(), "缺省写不该在仓库里建目录");

        let out = rt
            .block_on(memory_write(
                &c,
                &json!({"name": "接口约定", "content": "统一 snake_case", "scope": "project"}),
            ))
            .unwrap();
        assert!(out.contains("项目"), "{out}");
        assert!(project_dir(&cwd).join("接口约定.md").exists());
        assert!(project_dir(&cwd).join("MEMORY.md").exists(), "项目级索引要建");
        // 两级索引各记各的
        let gidx = std::fs::read_to_string(global_dir().join("MEMORY.md")).unwrap();
        assert!(gidx.contains("偏好") && !gidx.contains("接口约定"), "{gidx}");
        let pidx = std::fs::read_to_string(project_dir(&cwd).join("MEMORY.md")).unwrap();
        assert!(pidx.contains("接口约定") && !pidx.contains("偏好"), "{pidx}");

        // 认不出的 scope:报错,不静默落一处
        let err = rt
            .block_on(memory_write(&c, &json!({"name": "x", "content": "y", "scope": "weird"})))
            .unwrap_err();
        assert!(err.0.contains("scope 只能是"), "{err:?}");
    }

    /// 读:两级同名时都返回并标来源;显式 scope 只读那一级
    #[test]
    fn read_returns_both_levels_when_names_collide() {
        let (_g, cwd) = isolate("r");
        let perm = Permission::new(Mode::BypassPermissions);
        let c = ctx(&cwd, &perm);
        write_mem(global_dir(), "常用命令", "---\nname: 常用命令\n---\n\n全局版");
        write_mem(project_dir(&cwd), "常用命令", "---\nname: 常用命令\n---\n\n项目版");
        let rt = tokio::runtime::Runtime::new().unwrap();

        let both = rt
            .block_on(memory_read(&c, &json!({"name": "常用命令"})))
            .unwrap();
        assert!(both.contains("项目版") && both.contains("全局版"), "{both}");
        assert!(both.contains("项目记忆") && both.contains("全局记忆"), "{both}");
        assert!(
            both.find("项目版").unwrap() < both.find("全局版").unwrap(),
            "项目在前:{both}"
        );

        let only_global = rt
            .block_on(memory_read(&c, &json!({"name": "常用命令", "scope": "global"})))
            .unwrap();
        assert!(only_global.contains("全局版"));
        assert!(!only_global.contains("项目版"));
        assert!(!only_global.contains("###"), "只有一份时不加包装:{only_global}");

        let err = rt
            .block_on(memory_read(&c, &json!({"name": "没有这条"})))
            .unwrap_err();
        assert!(err.0.contains("未找到"), "{err:?}");
    }

    /// 搜索:缺省两级都搜并分组标注;显式一级就不加分组标题
    #[test]
    fn search_groups_by_scope() {
        let (_g, cwd) = isolate("s");
        let perm = Permission::new(Mode::BypassPermissions);
        let c = ctx(&cwd, &perm);
        write_mem(global_dir(), "g", "---\nname: 构建\n---\n\n全局: cargo build");
        write_mem(project_dir(&cwd), "p", "---\nname: 构建\n---\n\n项目: make build");
        let rt = tokio::runtime::Runtime::new().unwrap();

        let all = rt
            .block_on(memory_read(&c, &json!({"query": "build"})))
            .unwrap();
        assert!(all.contains("## 项目记忆") && all.contains("## 全局记忆"), "{all}");
        assert!(all.starts_with("找到 2 条记忆"), "{all}");

        let only = rt
            .block_on(memory_read(&c, &json!({"query": "build", "scope": "project"})))
            .unwrap();
        assert!(
            !only.contains("## 项目记忆") && !only.contains("## 全局记忆"),
            "只读一级不分组:{only}"
        );
        assert!(only.contains("项目: make build") && !only.contains("cargo build"), "{only}");

        let none = rt
            .block_on(memory_read(&c, &json!({"query": "不存在的东西"})))
            .unwrap();
        assert!(none.contains("没有找到"), "{none}");
    }

    /// 列表:两级合并、标签来源、项目在前;索引文件不计入
    #[test]
    fn list_merges_levels_and_tags_source() {
        let (_g, cwd) = isolate("list");
        write_mem(
            global_dir(),
            "machine",
            "---\nname: 机器硬件配置\ndescription: GPU 型号等\ntype: user\n---\n\nRTX 4090",
        );
        write_mem(global_dir(), "plain", "没有 frontmatter 的旧格式");
        std::fs::write(
            global_dir().join("MEMORY.md"),
            "- [机器硬件配置](machine.md) — GPU 型号等",
        )
        .unwrap();
        write_mem(project_dir(&cwd), "proj", "---\nname: 本仓约定\n---\nx");

        let list = list_memories(&cwd);
        assert_eq!(list.len(), 3, "两个索引文件都不计入");
        assert_eq!(list[0].source, Scope::Project, "项目在前:{list:?}");
        assert_eq!(list[0].name, "本仓约定");
        let machine = list.iter().find(|m| m.filename == "machine").unwrap();
        assert_eq!(machine.name, "机器硬件配置");
        assert_eq!(machine.description, "GPU 型号等");
        assert_eq!(machine.mtype, "user");
        assert_eq!(machine.source, Scope::Global);
        assert!(machine.size > 0);
        let plain = list.iter().find(|m| m.filename == "plain").unwrap();
        assert_eq!(plain.name, "(未命名)", "无 frontmatter 应兜底");

        // cwd 换一个:项目那份就看不见了(跟着目录走)
        let other = std::env::temp_dir().join(format!("znaide_mem_other_{}", std::process::id()));
        std::fs::create_dir_all(&other).unwrap();
        let list2 = list_memories(&other);
        assert!(
            list2.iter().all(|m| m.source == Scope::Global),
            "换个目录不该看到别的项目的记忆:{list2:?}"
        );
        assert_eq!(list2.len(), 2);
    }

    /// 删除:必须给 scope,只删那一级(同名另一级与它的索引都留着)
    #[test]
    fn delete_requires_scope_and_leaves_other_level_alone() {
        let (_g, cwd) = isolate("del");
        write_mem(global_dir(), "keep", "---\nname: 保留\ntype: project\n---\nx");
        write_mem(global_dir(), "same", "---\nname: 同名\ntype: user\n---\n全局");
        write_mem(project_dir(&cwd), "same", "---\nname: 同名\ntype: user\n---\n项目");
        std::fs::write(
            global_dir().join("MEMORY.md"),
            "- [保留](keep.md) — 1\n- [同名](same.md) — 2\n",
        )
        .unwrap();
        std::fs::write(project_dir(&cwd).join("MEMORY.md"), "- [同名](same.md) — 2\n").unwrap();

        assert!(delete_memory("same", Scope::Project, &cwd).unwrap());
        assert!(!project_dir(&cwd).join("same.md").exists(), "项目那份应删");
        assert!(global_dir().join("same.md").exists(), "全局同名不能被误删");
        let gidx = std::fs::read_to_string(global_dir().join("MEMORY.md")).unwrap();
        assert!(gidx.contains("same"), "全局索引行要留着:{gidx}");
        let pidx = std::fs::read_to_string(project_dir(&cwd).join("MEMORY.md")).unwrap();
        assert!(!pidx.contains("same"), "项目索引行该清掉:{pidx}");

        assert!(delete_memory("keep", Scope::Global, &cwd).unwrap());
        assert!(!global_dir().join("keep.md").exists());
        let gidx = std::fs::read_to_string(global_dir().join("MEMORY.md")).unwrap();
        assert!(!gidx.contains("keep") && gidx.contains("same"), "{gidx}");
        assert!(!delete_memory("keep", Scope::Global, &cwd).unwrap(), "再删返回 false");

        // 目录/文件不存在时列表为空、删除不崩
        let empty = std::env::temp_dir().join(format!("znaide_mem_none_{}", std::process::id()));
        std::fs::create_dir_all(&empty).unwrap();
        std::env::set_var("ZNAIDE_DATA_DIR", &empty);
        assert!(list_memories(&empty).is_empty());
        assert!(!delete_memory("ghost", Scope::Global, &empty).unwrap());
    }
}
