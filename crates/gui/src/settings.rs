//! 设置页的数据面:读快照、写配置文件。
//!
//! 只做"读/校验/写",不碰界面;所有写入都走 core 已有的 API(配置走
//! `Config::save` / `save_runtime_settings`,记忆/技能/人格走各自的路径规则),
//! 不在 GUI 里重造一套落盘逻辑。

use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use znaide_core::config::{Config, ProxyMode};
use znaide_core::skills::SkillSource;
use znaide_core::tools::memory::{self, Scope};

use crate::bridge::{MemoryForm, Status};

/// GUI 自己的偏好(不塞进 provider 配置):目前只记最后工作目录。
fn prefs_path() -> PathBuf {
    znaide_core::config::data_dir().join("gui.json")
}

pub fn last_cwd() -> Option<PathBuf> {
    let text = std::fs::read_to_string(prefs_path()).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    v.get("last_cwd")
        .and_then(|x| x.as_str())
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
}

pub fn save_last_cwd(cwd: &Path) {
    let _ = std::fs::create_dir_all(znaide_core::config::data_dir());
    let _ = std::fs::write(
        prefs_path(),
        json!({"last_cwd": cwd.display().to_string()}).to_string(),
    );
}

/// 用户主目录。Windows 上 `HOME` 通常没有(用的是 `USERPROFILE`),别去读环境变量。
pub fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(std::env::temp_dir)
}

/// 启动目录:--cwd > 上次选择 > 进程 cwd(排除 exe 目录/home/根) > home > 临时目录。
///
/// 返回的目录**一定存在**:Windows 上双击启动时进程 cwd 就是 exe 目录(被排除),
/// 而旧代码此时退回 `HOME` 环境变量 —— Windows 上没有这个变量,于是拿到空路径,
/// 引擎的 `prepare` 直接报"工作目录不存在"。
pub fn resolve_start_cwd(cli: Option<PathBuf>) -> PathBuf {
    if let Some(p) = cli {
        if p.is_dir() {
            return p;
        }
    }
    if let Some(p) = last_cwd() {
        return p;
    }
    let home = home_dir();
    let cur = std::env::current_dir().unwrap_or_else(|_| home.clone());
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|x| x.to_path_buf()));
    let unhelpful = cur == home
        || cur.parent().is_none()
        || exe_dir.as_ref().map(|d| d == &cur).unwrap_or(false);
    if cur.is_dir() && !unhelpful {
        return cur;
    }
    for cand in [home, exe_dir.unwrap_or_default(), cur] {
        if cand.is_dir() {
            return cand;
        }
    }
    std::env::temp_dir()
}

/// 系统目录选择器:Linux zenity/kdialog,Windows PowerShell,macOS osascript。
/// Err = 没有可用选择器(前端回退输入框);Ok(None) = 用户取消。
pub fn pick_directory() -> Result<Option<PathBuf>, String> {
    use std::process::Command;
    #[cfg(target_os = "linux")]
    {
        for cmd in ["zenity", "kdialog"] {
            let out = if cmd == "zenity" {
                Command::new(cmd)
                    .args(["--file-selection", "--directory"])
                    .output()
            } else {
                Command::new(cmd).args(["--getexistingdirectory"]).output()
            };
            match out {
                Ok(o) if o.status.success() => {
                    let p = String::from_utf8_lossy(&o.stdout).trim().to_string();
                    return Ok(if p.is_empty() {
                        None
                    } else {
                        Some(PathBuf::from(p))
                    });
                }
                Ok(_) => return Ok(None), // 用户取消
                Err(_) => continue,
            }
        }
        Err("没有 zenity/kdialog".into())
    }
    #[cfg(target_os = "windows")]
    {
        let script = "Add-Type -AssemblyName System.Windows.Forms; $d=New-Object System.Windows.Forms.FolderBrowserDialog; if($d.ShowDialog() -eq 'OK'){Write-Output $d.SelectedPath}";
        let mut cmd = Command::new("powershell");
        cmd.args(["-NoProfile", "-STA", "-Command", script]);
        // 无控制台的 GUI 里拉 powershell,不加这个标志会连带弹一个 cmd 黑框
        znaide_core::winproc::hide_console_window(&mut cmd);
        let out = cmd.output();
        match out {
            Ok(o) if o.status.success() => {
                let p = String::from_utf8_lossy(&o.stdout).trim().to_string();
                Ok(if p.is_empty() {
                    None
                } else {
                    Some(PathBuf::from(p))
                })
            }
            _ => Err("PowerShell 目录选择器不可用".into()),
        }
    }
    #[cfg(target_os = "macos")]
    {
        let out = Command::new("osascript")
            .args(["-e", "POSIX path of (choose folder)"])
            .output();
        match out {
            Ok(o) if o.status.success() => {
                let p = String::from_utf8_lossy(&o.stdout).trim().to_string();
                Ok(if p.is_empty() {
                    None
                } else {
                    Some(PathBuf::from(p))
                })
            }
            _ => Ok(None),
        }
    }
    // 其它目标(如 Android)没有系统目录选择器:让前端回退到输入框
    #[cfg(not(any(
        target_os = "linux",
        target_os = "windows",
        target_os = "macos"
    )))]
    {
        Err("当前平台没有系统目录选择器".into())
    }
}

/// 列出 cwd 下某一层路径(供 @ 补全)。返回 (相对路径,是否目录,是否截断)。
pub fn list_paths(cwd: &Path, prefix: &str) -> (Vec<(String, bool)>, bool) {
    let (dir_part, base) = match prefix.rfind('/') {
        Some(i) => (&prefix[..=i], &prefix[i + 1..]),
        None => ("", prefix),
    };
    let dir = if let Some(rest) = dir_part.strip_prefix("~/") {
        home_dir().join(rest)
    } else {
        let p = PathBuf::from(dir_part);
        if p.is_absolute() {
            p
        } else if dir_part.is_empty() {
            cwd.to_path_buf()
        } else {
            cwd.join(p)
        }
    };
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return (Vec::new(), false);
    };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.contains('"') || (name.starts_with('.') && !base.starts_with('.')) {
            continue;
        }
        if !name.to_lowercase().starts_with(&base.to_lowercase()) {
            continue;
        }
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let mut full = if dir_part.is_empty() {
            name
        } else {
            format!("{dir_part}{name}")
        };
        if is_dir {
            full.push('/');
        }
        out.push((full, is_dir));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    let truncated = out.len() > 200;
    out.truncate(200);
    (out, truncated)
}

/// 设置页整页快照。`status` 是运行中实际生效的那份(可能来自命令行覆盖)。
pub fn snapshot(cwd: &Path, cfg: &Config, status: &Status, update_notice: Option<&str>) -> Value {
    let provider = if status.provider.is_empty() {
        cfg.provider.clone().unwrap_or_else(|| "ollama".into())
    } else {
        status.provider.clone()
    };
    let resolved = cfg.resolve(None, None, None, Some(provider.clone())).ok();
    let model = status.model.clone();
    // 端点/Key 以"运行中实际生效"的为准(命令行覆盖不在 config 里)
    let base_url = if status.base_url.is_empty() {
        resolved
            .as_ref()
            .map(|r| r.base_url.clone())
            .unwrap_or_default()
    } else {
        status.base_url.clone()
    };
    let api_key_set =
        status.api_key_set || resolved.as_ref().and_then(|r| r.api_key.as_ref()).is_some();
    let ctx_window = resolved.as_ref().and_then(|r| r.effective_context_window());
    let user_provider = cfg.providers.get(&provider);
    let all_providers = cfg.all_providers();
    let provider_def = all_providers.get(&provider);
    // 配置里的“覆盖值”(None = 自动查表);与 effective 分开,避免保存时把自动值写死
    let ctx_fixed = user_provider
        .and_then(|p| p.context_window)
        .or(cfg.context_window);
    let provider_headers: Vec<Value> = user_provider
        .map(|p| {
            p.extra_headers
                .iter()
                .map(|(k, v)| json!({"name": k, "value": v}))
                .collect()
        })
        .unwrap_or_default();
    let provider_headers_enabled = user_provider.and_then(|p| p.extra_headers_enabled);

    let mut headers: Vec<Value> = cfg
        .extra_headers
        .iter()
        .map(|(k, v)| json!({"name": k, "value": v}))
        .collect();
    headers.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));

    let skills: Vec<Value> = znaide_core::skills::scan(cwd)
        .skills
        .iter()
        .map(|s| {
            json!({
                "name": s.name,
                "desc": s.description,
                "source": s.source.label(),
                "entry": s.entry.is_some(),
                "editable": !matches!(s.source, SkillSource::Builtin),
            })
        })
        .collect();

    let memories: Vec<Value> = memory::list_memories(cwd)
        .iter()
        .map(|m| {
            json!({
                "filename": m.filename,
                "name": m.name,
                "description": m.description,
                "mtype": m.mtype,
                "scope": match m.source {
                    Scope::Global => "global",
                    Scope::Project => "project",
                },
                "size": m.size,
                "modified": m.modified,
            })
        })
        .collect();

    let mcp_path = znaide_core::config::data_dir().join("mcp.json");
    let mcp_text = std::fs::read_to_string(&mcp_path).unwrap_or_default();

    let persona_texts: std::collections::HashMap<String, String> = znaide_core::persona::list()
        .into_iter()
        .filter_map(|name| znaide_core::persona::persona_text(&name).map(|text| (name, text)))
        .collect();

    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "build": "xiufan.0.0.1",
        "product": "修凡AI全能助手",
        "gui_version": env!("CARGO_PKG_VERSION"),
        "engine_version": znaide_core::VERSION,
        "cwd": cwd.display().to_string(),
        "config_path": znaide_core::config::config_path().display().to_string(),
        "data_dir": znaide_core::config::data_dir().display().to_string(),
        "provider": provider,
        "model": model,
        "configured": status.configured,
        "base_url": base_url,
        "api_key_set": api_key_set,
        "context_window": ctx_fixed,
        "context_window_effective": ctx_window,
        "api_key_env": provider_def.and_then(|p| p.api_key_env.clone()),
        "provider_headers": provider_headers,
        "provider_headers_enabled": provider_headers_enabled,
        "max_turns": cfg.max_turns,
        "retry": cfg.retry.unwrap_or(0),
        "proxy": {
            "mode": match cfg.proxy.mode {
                ProxyMode::Auto => "auto",
                ProxyMode::Direct => "direct",
                ProxyMode::Manual => "manual",
            },
            "url": cfg.proxy.url.clone().unwrap_or_default(),
        },
        "headers_enabled": cfg.extra_headers_enabled != Some(false),
        "headers": headers,
        "providers": cfg.list_providers().into_iter().map(|(name, base, model)| {
            json!({"name": name, "base_url": base, "model": model})
        }).collect::<Vec<_>>(),
        "personas": znaide_core::persona::list(),
        "persona_texts": persona_texts,
        "persona": status.persona,
        "skills": skills,
        "memories": memories,
        "mcp_text": mcp_text,
        "mcp_path": mcp_path.display().to_string(),
        "update_notice": update_notice,
    })
}

/// 写一个自定义人格(`~/.znaide/personas/<名字>.md`)。
pub fn save_persona(name: &str, body: &str) -> anyhow::Result<PathBuf> {
    let name = name.trim();
    if name.is_empty() {
        anyhow::bail!("人格名不能为空");
    }
    if name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        anyhow::bail!("人格名不能包含路径分隔符");
    }
    let dir = znaide_core::persona::user_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{name}.md"));
    std::fs::write(&path, body)?;
    Ok(path)
}

/// 读技能正文(统一是目录里的 SKILL.md)。
pub fn read_skill(cwd: &Path, name: &str) -> anyhow::Result<(PathBuf, String, bool)> {
    let set = znaide_core::skills::scan(cwd);
    let skill = set
        .skills
        .iter()
        .find(|s| s.name == name)
        .ok_or_else(|| anyhow::anyhow!("没有这个技能:{name}"))?;
    let path = skill.dir.join("SKILL.md");
    let body = std::fs::read_to_string(&path).unwrap_or_default();
    Ok((path, body, !matches!(skill.source, SkillSource::Builtin)))
}

/// 保存技能正文(内置只读)。
pub fn save_skill(cwd: &Path, name: &str, body: &str) -> anyhow::Result<PathBuf> {
    let (path, _old, editable) = read_skill(cwd, name)?;
    if !editable {
        anyhow::bail!("内置技能只读,请在 ~/.znaide/skills 或 .znaide/skills 新建同名技能覆盖");
    }
    std::fs::write(&path, body)?;
    Ok(path)
}

/// 找一条记忆的元信息(编辑时保留 name/description/type)。
pub fn memory_meta(cwd: &Path, filename: &str, scope: &str) -> Option<memory::MemoryEntry> {
    let want = Scope::parse(scope)?;
    memory::list_memories(cwd)
        .into_iter()
        .find(|m| m.filename == filename && m.source == want)
}

/// 读记忆正文。
pub fn read_memory(cwd: &Path, filename: &str, scope: &str) -> anyhow::Result<String> {
    let scope = Scope::parse(scope).ok_or_else(|| anyhow::anyhow!("未知作用域:{scope}"))?;
    let path = memory::memory_path(filename, scope, cwd)
        .ok_or_else(|| anyhow::anyhow!("记忆名不合法:{filename}"))?;
    Ok(std::fs::read_to_string(path)?)
}

/// 新建/覆盖一条记忆:GUI 侧表单 → frontmatter + 正文。
pub fn save_memory(cwd: &Path, form: &MemoryForm) -> anyhow::Result<PathBuf> {
    let scope = Scope::parse(&form.scope).ok_or_else(|| anyhow::anyhow!("未知作用域"))?;
    let name = form.name.trim();
    if name.is_empty() {
        anyhow::bail!("记忆名不能为空");
    }
    let filename = if form.filename.trim().is_empty() {
        name
    } else {
        form.filename.trim()
    };
    let path = memory::memory_path(filename, scope, cwd)
        .ok_or_else(|| anyhow::anyhow!("记忆名不合法:{filename}"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let one_line = |s: &str| s.replace(['\n', '\r'], " ").trim().to_string();
    let text = format!(
        "---\nname: {}\ndescription: {}\ntype: {}\n---\n\n{}\n",
        one_line(name),
        one_line(&form.description),
        one_line(&form.mtype),
        form.body.trim()
    );
    std::fs::write(&path, text)?;
    Ok(path)
}

/// 保存 mcp.json 原文(先校验是 JSON 对象,别把坏文件写进去)。
pub fn save_mcp(text: &str) -> anyhow::Result<PathBuf> {
    let v: Value =
        serde_json::from_str(text).map_err(|e| anyhow::anyhow!("mcp.json 不是合法 JSON:{e}"))?;
    if !v.is_object() {
        anyhow::bail!("mcp.json 顶层必须是对象(如 {{\"mcpServers\": {{...}}}})");
    }
    let path = znaide_core::config::data_dir().join("mcp.json");
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, format!("{}\n", serde_json::to_string_pretty(&v)?))?;
    Ok(path)
}

/// 供 `main` 判断某条记忆/技能动作是否合法用的小工具。
pub fn parse_scope(scope: &str) -> Option<Scope> {
    Scope::parse(scope)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_cwd_always_exists() {
        // Windows 双击时进程 cwd = exe 目录(被排除),曾经会退化成空路径
        let p = resolve_start_cwd(None);
        assert!(p.is_dir(), "启动目录必须存在,得到 {p:?}");
    }

    #[test]
    fn home_dir_exists() {
        assert!(home_dir().is_dir(), "主目录必须存在,得到 {:?}", home_dir());
    }

    #[test]
    fn list_paths_lists_cwd_entries() {
        let dir = std::env::temp_dir().join(format!("xf_paths_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("abc")).unwrap();
        std::fs::write(dir.join("abd.txt"), "x").unwrap();
        let (items, _truncated) = list_paths(&dir, "ab");
        assert!(
            items.iter().any(|(p, is_dir)| p == "abc/" && *is_dir),
            "{items:?}"
        );
        assert!(items.iter().any(|(p, _)| p == "abd.txt"), "{items:?}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
