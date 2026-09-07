//! 人格(persona):全局注入 system prompt 的身份段。
//!
//! 内置预设 + `~/.znaide/personas/<名字>.md` 自定义(文件正文即人设说明,
//! 与 skills 同款文档驱动)。人格只影响 AI 的表达、语气与看待任务的视角;
//! 工作守则与安全边界作为底层约束独立保留(见 build_system_prompt),绝不让
//! 人格覆盖工具纪律——这是安全底线。

/// 内置人格:名字 → 人设说明。文案从仓库 personas示例/ 编译期内嵌
/// (改了示例目录即改内置,与技能示例同模式);启动时物化到
/// ~/.znaide/personas/ 的可编辑副本,改动以文件为准、删除回退内置。
pub const BUILTIN: &[(&str, &str)] = &[
    (
        "毒舌损友",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../personas示例/毒舌损友.md"
        )),
    ),
    (
        "耐心老师",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../personas示例/耐心老师.md"
        )),
    ),
    (
        "热血极客",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../personas示例/热血极客.md"
        )),
    ),
    (
        "极简冷淡风",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../personas示例/极简冷淡风.md"
        )),
    ),
];

/// 用户自定义人格目录
pub fn user_dir() -> std::path::PathBuf {
    crate::config::data_dir().join("personas")
}

/// 把内置人格同步为 ~/.znaide/personas/<名字>.md(缺失才写,不覆盖用户
/// 改动)。文件内容与人设正文**一字不差**、不加任何说明——改动即生效,
/// 删除即回退代码内置,都是行为语义,不用写进文件里。
/// 旧版曾写过带 "# 说明头" 的文件,检测到就重写干净(仅当正文仍是内置原样)。
pub fn ensure_builtin_files() {
    let dir = user_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    for (name, text) in BUILTIN {
        let path = dir.join(format!("{name}.md"));
        if !path.exists() {
            let _ = std::fs::write(&path, format!("{text}\n"));
            continue;
        }
        // 旧版注释头残留:正文仍是内置原样 → 清掉说明,只留正文
        if let Ok(cur) = std::fs::read_to_string(&path) {
            if cur.trim_start().starts_with("# znaide 内置人格")
                && strip_comments(&cur) == *text
            {
                let _ = std::fs::write(&path, format!("{text}\n"));
            }
        }
    }
}

/// 剥掉以 # 开头的行为注释(自定义文件允许带说明行;正文取非注释行)
fn strip_comments(text: &str) -> String {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// 列出所有可用人格名(内置 + 用户自定义;用户同名文件覆盖内置)。
pub fn list() -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(user_dir()) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("md") {
                if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                    names.push(stem.to_string());
                }
            }
        }
    }
    for (name, _) in BUILTIN {
        if !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    }
    names.sort();
    names
}

/// 取人格注入文本:用户自定义文件优先(剥注释),否则内置;没有返回 None
pub fn persona_text(name: &str) -> Option<String> {
    let file = user_dir().join(format!("{name}.md"));
    if file.is_file() {
        if let Ok(text) = std::fs::read_to_string(&file) {
            let text = strip_comments(&text);
            if !text.is_empty() {
                return Some(text);
            }
        }
    }
    BUILTIN
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, text)| text.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_personas_resolvable() {
        assert_eq!(BUILTIN.len(), 4);
        for (name, text) in BUILTIN {
            assert!(!name.is_empty());
            assert!(text.chars().count() > 10, "{name} 的人设说明太短");
        }
        for name in ["毒舌损友", "耐心老师", "热血极客", "极简冷淡风"] {
            assert!(persona_text(name).is_some(), "{name} 应能解析出文本");
            assert!(list().iter().any(|n| n == name));
        }
        // 未知人格返回 None
        assert!(persona_text("不存在的角色").is_none());
    }

    /// 注释头(# 行)不参与注入;正文保留
    #[test]
    fn strip_comment_header() {
        let text = "# 说明头\n# 第二行注释\n\n  正文第一句。\n  正文第二句。\n";
        assert_eq!(strip_comments(text), "正文第一句。\n  正文第二句。");
    }
}
