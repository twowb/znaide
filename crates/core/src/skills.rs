//! skill 能力包:模型可自主调用或 `/技能名` 手动触发。
//! 一个技能 = `<名字>/SKILL.md`(frontmatter + 正文,正文教模型怎么做,支持 {args} 占位),
//! 可带 scripts/ 附件和可选 entry 入口脚本(调用时自动执行一次,输出并入上下文)。
//! frontmatter:name(须与目录名一致)/ description / disable-model-invocation(true 则只能 /名字 触发)/ entry。
//!
//! 层级:内置(编译期示例,启动物化到 ~/.znaide/builtin-skills/)> 用户
//! ~/.znaide/skills/ > 项目 <cwd>/.znaide/skills/;同名项目覆盖用户、用户覆盖内置。
use crate::config::data_dir;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// 内置示例技能(archive-downloads / clean-junk / weekly-report):文件内容
/// 编译期从仓库 skills示例/ 内嵌——改了示例目录即改了内置,单一来源。
/// 启动时物化到 ~/.znaide/builtin-skills/(缺失才写,用户可改);带入口脚本
/// 的技能脚本一起物化,entry 相对路径原样可用。
const BUILTIN_ASSETS: &[(&str, &[(&str, &str)])] = &[
    (
        "archive-downloads",
        &[
            (
                "SKILL.md",
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../skills示例/archive-downloads/SKILL.md"
                )),
            ),
            (
                "scripts/snapshot.sh",
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../skills示例/archive-downloads/scripts/snapshot.sh"
                )),
            ),
            (
                "scripts/snapshot.ps1",
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../skills示例/archive-downloads/scripts/snapshot.ps1"
                )),
            ),
        ],
    ),
    (
        "clean-junk",
        &[
            (
                "SKILL.md",
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../skills示例/clean-junk/SKILL.md"
                )),
            ),
            (
                "scripts/scan.sh",
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../skills示例/clean-junk/scripts/scan.sh"
                )),
            ),
            (
                "scripts/scan.ps1",
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../skills示例/clean-junk/scripts/scan.ps1"
                )),
            ),
        ],
    ),
    (
        "weekly-report",
        &[(
            "SKILL.md",
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../skills示例/weekly-report/SKILL.md"
            )),
        )],
    ),
];

/// 内置技能物化目录
pub fn builtin_dir() -> PathBuf {
    data_dir().join("builtin-skills")
}

/// 把内置示例技能物化到 ~/.znaide/builtin-skills/(缺失才写,不覆盖改动)。
/// 之后 skills 开箱即有:模型可自主调用、也可 /名字 手动触发。
pub fn ensure_builtin_skills() {
    let root = builtin_dir();
    if std::fs::create_dir_all(&root).is_err() {
        return;
    }
    for (name, files) in BUILTIN_ASSETS {
        for (rel, content) in *files {
            let path = root.join(name).join(rel);
            if path.exists() {
                continue;
            }
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&path, content);
        }
    }
}

/// skill 来源层级
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSource {
    Builtin,
    User,
    Project,
}

impl SkillSource {
    pub fn label(self) -> &'static str {
        match self {
            SkillSource::Builtin => "内置",
            SkillSource::User => "用户",
            SkillSource::Project => "项目",
        }
    }
}

/// 一个已加载的 skill
#[derive(Debug, Clone)]
pub struct Skill {
    /// 规范名 = 目录名(唯一标识,也用于 /命令 与模型 enum)
    pub name: String,
    pub description: String,
    /// true = 不暴露给模型,只能手动 /名字 触发
    pub disable_model_invocation: bool,
    /// 入口脚本相对路径(skill 目录内),None = 无入口
    pub entry: Option<String>,
    /// 正文模板(可含 {args})
    pub body: String,
    /// SKILL.md 所在目录
    pub dir: PathBuf,
    pub source: SkillSource,
}

impl Skill {
    /// 把正文里的 {args} 替换为实际参数
    pub fn render(&self, args: &str) -> String {
        self.body.replace("{args}", args)
    }

    /// 入口脚本的绝对路径(entry 存在时)
    pub fn entry_abs(&self) -> Option<PathBuf> {
        self.entry.as_ref().map(|e| self.dir.join(e))
    }
}

/// 一次扫描的结果:按名排序的技能列表 + 解析告警(坏文件等)
#[derive(Debug, Default)]
pub struct SkillSet {
    pub skills: Vec<Skill>,
    pub warnings: Vec<String>,
}

/// 生产入口:内置(builtin-skills)→ 用户 → 项目,上层同名覆盖下层
pub fn scan(cwd: &Path) -> SkillSet {
    let mut map: BTreeMap<String, Skill> = BTreeMap::new();
    let mut warnings: Vec<String> = Vec::new();
    scan_dir_into(&mut map, &mut warnings, &builtin_dir(), SkillSource::Builtin);
    scan_dir_into(&mut map, &mut warnings, &data_dir().join("skills"), SkillSource::User);
    scan_dir_into(&mut map, &mut warnings, &cwd.join(".znaide").join("skills"), SkillSource::Project);
    finish_skill_set(map, warnings)
}

/// 扫任意两个目录(测试注入用),project 层覆盖 user 层同名技能
pub fn scan_with_dirs(user_dir: &Path, project_dir: &Path) -> SkillSet {
    let mut map: BTreeMap<String, Skill> = BTreeMap::new();
    let mut warnings: Vec<String> = Vec::new();
    scan_dir_into(&mut map, &mut warnings, user_dir, SkillSource::User);
    scan_dir_into(&mut map, &mut warnings, project_dir, SkillSource::Project);
    finish_skill_set(map, warnings)
}

/// 扫单个技能目录入 map(同名直接覆盖,后扫的赢)
fn scan_dir_into(
    map: &mut BTreeMap<String, Skill>,
    warnings: &mut Vec<String>,
    dir: &Path,
    source: SkillSource,
) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let sdir = e.path();
        if !sdir.is_dir() {
            continue;
        }
        match load_skill(&sdir, source) {
            Ok(skill) => {
                map.insert(skill.name.clone(), skill);
            }
            Err(w) => warnings.push(w),
        }
    }
}

fn finish_skill_set(map: BTreeMap<String, Skill>, mut warnings: Vec<String>) -> SkillSet {
    let mut skills: Vec<Skill> = map.into_values().collect();
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    warnings.sort();
    SkillSet { skills, warnings }
}

/// 技能名合法性:字母/数字/下划线/连字符,不以连字符开头
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 40
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// 从 <name>/SKILL.md 加载一个 skill
fn load_skill(dir: &Path, source: SkillSource) -> Result<Skill, String> {
    let name = dir
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    if !valid_name(&name) {
        return Err(format!("跳过无效技能目录名: {name:?}({} 级)", source.label()));
    }
    let md_path = dir.join("SKILL.md");
    let text = match std::fs::read_to_string(&md_path) {
        Ok(t) => t,
        Err(_) => return Err(format!("跳过 {}(无 SKILL.md)", dir.display())),
    };
    let (fm, body) = parse_frontmatter(&text);
    // frontmatter 里若写了 name,须与目录名一致
    if let Some(fm_name) = &fm.name {
        if fm_name.as_str() != name {
            return Err(format!(
                "跳过 {}:frontmatter name 为 {fm_name:?},与目录名 {name:?} 不一致",
                md_path.display()
            ));
        }
    }
    // entry 必须是指向 skill 目录内的相对路径
    let entry = match &fm.entry {
        Some(e) => {
            let p = Path::new(e);
            if p.is_absolute() || p.components().any(|c| c == std::path::Component::ParentDir) {
                return Err(format!(
                    "跳过 {}:entry 必须是技能目录内的相对路径,不能含 ..",
                    md_path.display()
                ));
            }
            Some(e.clone())
        }
        None => None,
    };
    Ok(Skill {
        name,
        description: fm.description,
        disable_model_invocation: fm.disable_model_invocation,
        entry,
        body,
        dir: dir.to_path_buf(),
        source,
    })
}

/// frontmatter 简子集(不引 yaml 依赖,与旧 commands 解析同风格)
#[derive(Debug, Default)]
struct Frontmatter {
    name: Option<String>,
    description: String,
    disable_model_invocation: bool,
    entry: Option<String>,
}

/// 解析 SKILL.md:frontmatter 字段 + 正文(去除首尾空白)
fn parse_frontmatter(text: &str) -> (Frontmatter, String) {
    let mut fm = Frontmatter::default();
    if let Some(rest) = text.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            let fm_text = &rest[..end];
            let body = rest[end + 4..].trim().to_string();
            for line in fm_text.lines() {
                let line = line.trim();
                if let Some(v) = line.strip_prefix("name:") {
                    fm.name = Some(v.trim().trim_matches('"').trim_matches('\'').to_string());
                } else if let Some(v) = line.strip_prefix("description:") {
                    fm.description = v.trim().trim_matches('"').trim_matches('\'').to_string();
                } else if let Some(v) = line.strip_prefix("disable-model-invocation:") {
                    fm.disable_model_invocation = v.trim().eq_ignore_ascii_case("true");
                } else if let Some(v) = line.strip_prefix("entry:") {
                    fm.entry = Some(v.trim().trim_matches('"').trim_matches('\'').to_string());
                }
            }
            return (fm, body);
        }
    }
    (fm, text.trim().to_string())
}

/// 模型可自主调用的技能(过滤 disable_model_invocation),供 enum 生成
pub fn model_visible(skills: &[Skill]) -> Vec<&Skill> {
    skills
        .iter()
        .filter(|s| !s.disable_model_invocation)
        .collect()
}

// ---- entry 命令构造(跨平台) ----
// unix 用 bash 跑 .sh(不要求 +x/shebang);Windows 上声明 .sh 时会按 .ps1 → .bat → .cmd
// 顺序在同目录找同名脚本,找到就用,全没有才退回原 .sh(要求系统里装了 bash)。
// 返回 Err = 该技能在当前平台没有可用入口,调用方提示一声、只按正文执行。

#[cfg(unix)]
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Windows 下应尝试的候选入口名(相对 skill 目录,纯逻辑便于跨平台单测)
pub fn win_entry_candidates(declared: &str) -> Vec<String> {
    let p = Path::new(declared);
    match p.extension().and_then(|x| x.to_str()) {
        Some("ps1") | Some("bat") | Some("cmd") => vec![declared.to_string()],
        _ => {
            let parent = p.parent().filter(|d| !d.as_os_str().is_empty());
            let stem = p
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let mut out = Vec::new();
            for ext in ["ps1", "bat", "cmd"] {
                let cand = match &parent {
                    Some(dir) => {
                        let mut pb = dir.to_path_buf();
                        pb.push(format!("{stem}.{ext}"));
                        pb.to_string_lossy().to_string()
                    }
                    None => format!("{stem}.{ext}"),
                };
                out.push(cand);
            }
            out.push(declared.to_string());
            out
        }
    }
}

/// unix(macOS/Linux):bash -- <entry> <args>
#[cfg(unix)]
pub fn entry_command(skill: &Skill, args: &str) -> Result<String, String> {
    let declared = skill
        .entry
        .as_ref()
        .ok_or_else(|| "技能未声明 entry".to_string())?;
    let entry = skill.dir.join(declared);
    if !entry.is_file() {
        return Err(format!("入口脚本不存在: {}", entry.display()));
    }
    Ok(format!(
        "SKILL_DIR={} bash -- {} {}",
        sh_quote(&skill.dir.to_string_lossy()),
        sh_quote(&entry.to_string_lossy()),
        sh_quote(args)
    ))
}

#[cfg(windows)]
fn win_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Windows:cmd /C 通道。.ps1 用 powershell 执行;.bat/.cmd 用 call;其它直接运行。
#[cfg(windows)]
pub fn entry_command(skill: &Skill, args: &str) -> Result<String, String> {
    let declared = skill
        .entry
        .as_ref()
        .ok_or_else(|| "技能未声明 entry".to_string())?;
    let mut chosen: Option<PathBuf> = None;
    for cand in win_entry_candidates(declared) {
        let p = skill.dir.join(&cand);
        if p.is_file() {
            chosen = Some(p);
            break;
        }
    }
    let entry = chosen.ok_or_else(|| {
        format!(
            "当前平台(Windows)没有可用的入口脚本:在技能目录里找不到 {} 及其 .ps1/.bat/.cmd 版本",
            declared
        )
    })?;
    let entry_s = entry.to_string_lossy().to_string();
    let lower = entry_s.to_ascii_lowercase();
    let runner = if lower.ends_with(".ps1") {
        format!(
            "powershell -NoProfile -ExecutionPolicy Bypass -File {} {}",
            win_quote(&entry_s),
            win_quote(args)
        )
    } else if lower.ends_with(".bat") || lower.ends_with(".cmd") {
        format!("call {} {}", win_quote(&entry_s), win_quote(args))
    } else {
        format!("{} {}", win_quote(&entry_s), win_quote(args))
    };
    Ok(format!(
        "set \"SKILL_DIR={}\"&& {}",
        skill.dir.to_string_lossy().replace('"', "\"\""),
        runner
    ))
}

// ---- commands 迁移(统一进 skill 体系) ----

/// 把旧 ~/.znaide/commands/*.md 平移成 skills/<名>/SKILL.md。
/// 前提:skills 目录为空(不覆盖已有技能);迁移后默认 disable-model-invocation: true,
/// 保住"仅手动 /命令"的旧语义。返回迁移数量。目录可注入,便于测试。
pub fn migrate_legacy_commands() -> usize {
    migrate_commands_from(&data_dir().join("commands"), &data_dir().join("skills"))
}

fn migrate_commands_from(commands_dir: &Path, skills_dir: &Path) -> usize {
    let Ok(rd) = std::fs::read_dir(commands_dir) else {
        return 0;
    };
    let md_files: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("md"))
        .collect();
    if md_files.is_empty() {
        return 0;
    }
    if let Ok(rd) = std::fs::read_dir(skills_dir) {
        if rd.flatten().next().is_some() {
            return 0; // skills 已有内容,不自动迁移,避免覆盖
        }
    }
    let mut migrated = 0;
    for src in &md_files {
        let name = src.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        if !valid_name(&name) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(src) else {
            continue;
        };
        let (_, body) = parse_frontmatter(&text);
        let desc = extract_description(&text);
        let dest_dir = skills_dir.join(&name);
        if std::fs::create_dir_all(&dest_dir).is_err() {
            continue;
        }
        let dest = dest_dir.join("SKILL.md");
        let new_text = format!(
            "---\ndescription: {desc}\ndisable-model-invocation: true\n---\n\n{body}"
        );
        if std::fs::write(&dest, new_text).is_err() {
            continue;
        }
        let _ = std::fs::remove_file(src);
        migrated += 1;
    }
    migrated
}

/// 从旧命令文本里读 description(走 parse_frontmatter,别再手抄一份同样的解析)
fn extract_description(text: &str) -> String {
    parse_frontmatter(text).0.description
}

/// 按名精确查找(含手动触发与模型调用两种入口)
pub fn find<'a>(skills: &'a [Skill], name: &str) -> Option<&'a Skill> {
    skills.iter().find(|s| s.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 内置资产完整:三个示例技能都在,SKILL.md 可解析,entry 声明引用的
    /// 脚本也一并内嵌(否则物化后入口缺失,技能调用会挂)
    #[test]
    fn builtin_assets_complete() {
        let names: Vec<&str> = BUILTIN_ASSETS.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, vec!["archive-downloads", "clean-junk", "weekly-report"]);
        for (name, files) in BUILTIN_ASSETS {
            let md = files
                .iter()
                .find(|(p, _)| *p == "SKILL.md")
                .expect("每个内置技能都要有 SKILL.md");
            assert!(md.1.trim_start().starts_with("---"), "{name} 应带 frontmatter");
            // 手工解析 entry 行(frontmatter 简子集,与 load_skill 同规则)
            if let Some(rest) = md.1.strip_prefix("---") {
                if let Some(end) = rest.find("\n---") {
                    let fm_text = &rest[..end];
                    if let Some(entry) = fm_text
                        .lines()
                        .find_map(|l| l.trim().strip_prefix("entry:"))
                    {
                        let entry = entry.trim().trim_matches('"').trim_matches('\'').to_string();
                        assert!(
                            files.iter().any(|(p, _)| *p == entry),
                            "{name}:entry 脚本 {entry} 未随内置物化"
                        );
                    }
                }
            }
        }
    }

    fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
        let d = dir.join(name);
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("SKILL.md");
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn parse_frontmatter_fields_and_body() {
        let text = "---\nname: demo\ndescription: 测试技能\nentry: scripts/run.sh\ndisable-model-invocation: true\n---\n\n请按步骤做 {args}";
        let (fm, body) = parse_frontmatter(text);
        assert_eq!(fm.name.as_deref(), Some("demo"));
        assert_eq!(fm.description, "测试技能");
        assert_eq!(fm.entry.as_deref(), Some("scripts/run.sh"));
        assert!(fm.disable_model_invocation);
        assert_eq!(body, "请按步骤做 {args}");
    }

    #[test]
    fn render_replaces_args() {
        let text = "---\ndescription: x\n---\n先做 {args},再做 {args}";
        let (_, body) = parse_frontmatter(text);
        assert_eq!(body.replace("{args}", "A"), "先做 A,再做 A");
    }

    #[test]
    fn scan_project_overrides_user_and_filters_disable() {
        let tmp = std::env::temp_dir().join(format!("znaide_skills_{}", std::process::id()));
        let user = tmp.join("user");
        let proj = tmp.join("proj");
        write(&user, "alpha", "---\ndescription: a\n---\n用户版 alpha");
        write(
            &user,
            "beta",
            "---\ndescription: b\ndisable-model-invocation: true\n---\nbeta",
        );
        write(&proj, "alpha", "---\ndescription: a2\n---\n项目版 alpha");
        write(&proj, "gamma", "---\ndescription: g\n---\ngamma");
        // 坏目录:名字非法 / 无 SKILL.md / name 不一致
        std::fs::create_dir_all(tmp.join("user/1bad")).unwrap();
        std::fs::create_dir_all(tmp.join("user/empty")).unwrap();
        write(&user, "alpha", "---\nname: other\n---\nx"); // 覆盖为 name 不一致 → 产生告警

        let set = scan_with_dirs(&user, &proj);
        // 项目覆盖用户:alpha 是项目版
        let alpha = find(&set.skills, "alpha").unwrap();
        assert_eq!(alpha.body, "项目版 alpha");
        assert_eq!(alpha.source, SkillSource::Project);
        assert_eq!(set.skills.len(), 3); // alpha(项目)+ beta + gamma
        // beta disable → 不出现在模型可见列表
        let visible = model_visible(&set.skills);
        assert!(visible.iter().all(|s| s.name != "beta"));
        assert_eq!(visible.len(), 2);
        // 告警:坏目录与 name 不一致
        assert!(
            set.warnings.iter().any(|w| w.contains("1bad") || w.contains("other")),
            "warnings: {:?}",
            set.warnings
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn entry_must_be_relative_and_inner() {
        let tmp = std::env::temp_dir().join(format!("znaide_skills2_{}", std::process::id()));
        let user = tmp.join("user");
        write(&user, "ok", "---\ndescription: e\nentry: scripts/run.sh\n---\nx");
        let set = scan_with_dirs(&user, &tmp.join("empty"));
        assert!(set.skills[0].entry_abs().unwrap().ends_with("scripts/run.sh"));
        // 绝对路径 / 含 .. 的 entry → 整包拒绝
        write(&user, "badabs", "---\ndescription: e\nentry: /etc/passwd\n---\nx");
        write(&user, "badup", "---\ndescription: e\nentry: ../evil.sh\n---\nx");
        let set = scan_with_dirs(&user, &tmp.join("empty"));
        assert!(!set.skills.iter().any(|s| s.name == "badabs"));
        assert!(!set.skills.iter().any(|s| s.name == "badup"));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[cfg(unix)]
    #[test]
    fn entry_command_builds_quoted_bash() {
        // 用真实存在的临时技能目录(entry_command 现在校验脚本存在)
        let tmp = std::env::temp_dir().join(format!("znaide_entry_{}", std::process::id()));
        let dir = tmp.join("demo");
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::write(dir.join("scripts/run.sh"), "#!/usr/bin/env bash\necho hi\n").unwrap();
        let skill = Skill {
            name: "demo".into(),
            description: String::new(),
            disable_model_invocation: false,
            entry: Some("scripts/run.sh".into()),
            body: String::new(),
            dir,
            source: SkillSource::User,
        };
        let cmd = entry_command(&skill, "arg with 'quote'").unwrap();
        assert!(cmd.starts_with("SKILL_DIR='"));
        assert!(cmd.contains("bash -- "));
        // 路径与参数都被单引号包住,内部引号以 '\'' 转义
        assert!(cmd.contains("'\\''quote'\\'''"), "cmd: {cmd}");
        assert!(cmd.ends_with('\''));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[cfg(unix)]
    #[test]
    fn entry_command_missing_script_is_error() {
        let tmp = std::env::temp_dir().join(format!("znaide_entry2_{}", std::process::id()));
        let dir = tmp.join("demo");
        std::fs::create_dir_all(&dir).unwrap();
        let skill = Skill {
            name: "demo".into(),
            description: String::new(),
            disable_model_invocation: false,
            entry: Some("scripts/nope.sh".into()),
            body: String::new(),
            dir,
            source: SkillSource::User,
        };
        let err = entry_command(&skill, "").unwrap_err();
        assert!(err.contains("入口脚本不存在"), "err: {err}");
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn windows_entry_candidates_prefer_platform_scripts() {
        // .sh 声明 → 找同名 .ps1/.bat/.cmd,最后才退回原 .sh
        assert_eq!(
            win_entry_candidates("scripts/scan.sh"),
            vec![
                "scripts/scan.ps1".to_string(),
                "scripts/scan.bat".to_string(),
                "scripts/scan.cmd".to_string(),
                "scripts/scan.sh".to_string(),
            ]
        );
        // 显式平台脚本 → 直接用
        assert_eq!(win_entry_candidates("scripts/scan.ps1"), vec!["scripts/scan.ps1"]);
        assert_eq!(win_entry_candidates("scripts/scan.bat"), vec!["scripts/scan.bat"]);
    }

    #[test]
    fn migrate_commands_when_skills_empty() {
        let tmp = std::env::temp_dir().join(format!("znaide_mig_{}", std::process::id()));
        let cmd_dir = tmp.join("commands");
        let skill_dir = tmp.join("skills");
        std::fs::create_dir_all(&cmd_dir).unwrap();
        std::fs::write(
            cmd_dir.join("sum.md"),
            "---\ndescription: 总结代码\n---\n请总结 {args} 的代码",
        )
        .unwrap();
        std::fs::write(cmd_dir.join("ping.md"), "---\ndescription: ping\n---\nping {args}").unwrap();

        // 1) skills 为空 → 迁移
        assert_eq!(migrate_commands_from(&cmd_dir, &skill_dir), 2);
        let set = scan_with_dirs(&skill_dir, &tmp.join("empty"));
        assert_eq!(set.skills.len(), 2);
        let sum = find(&set.skills, "sum").unwrap();
        assert!(sum.disable_model_invocation); // 保住"仅手动"语义
        assert_eq!(sum.description, "总结代码");
        assert_eq!(sum.render("src/main.rs"), "请总结 src/main.rs 的代码");
        assert!(!cmd_dir.join("sum.md").exists()); // 源文件已移走

        // 2) skills 已有内容 → 不再迁移(不覆盖)
        let new_file = skill_dir.join("manual");
        std::fs::create_dir_all(&new_file).unwrap();
        std::fs::write(new_file.join("SKILL.md"), "---\ndescription: m\n---\nm").unwrap();
        std::fs::write(cmd_dir.join("late.md"), "---\ndescription: l\n---\nlate").unwrap();
        assert_eq!(migrate_commands_from(&cmd_dir, &skill_dir), 0);
        assert!(cmd_dir.join("late.md").exists());

        std::fs::remove_dir_all(&tmp).ok();
    }
}
