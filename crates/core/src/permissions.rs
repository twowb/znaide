//! 权限档位与命令安全判定。
//!
//! 判定思路(启发式,不是安全边界):不再对命令原文做子串匹配,而是
//!   词法切分 → 简单命令段 → 字面变量替换 → argv
//!   → 按命令分派规则(flag 集合 + 目标路径)→ 路径规范化后比较
//!
//! 因此空格数量、flag 顺序与合并写法、引号写法都不影响判定;目标是具体路径
//! 时不会被硬拒(`rm -rf /tmp/123` 放行、`rm  -rf /` 照样拦)。
//!
//! 边界:文本层无法判定变量间接(`T=/` 之后再 `$T` 之外的赋值)、命令替换、
//! `xargs`/解释器之外的间接调用,以及执行时才产生的值。真正的隔离需要内核
//! 机制(见 docs/plan-sandbox.md)。对外口径是"启发式识别",不是"安全拦截"。

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

/// 权限模式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 默认:写文件/执行命令先问(无头模式没法问,直接拒)
    Ask,
    /// 改文件自动放行,命令仍要问
    AcceptEdits,
    /// 全放行,高危命令仍要人工确认(命中时不自动放行)
    BypassPermissions,
    /// YOLO:一切放行、高危判定也跳过。只该在完全信任模型的自用场景开。
    Yolo,
}

impl Mode {
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        match s {
            "ask" => Ok(Mode::Ask),
            "acceptEdits" => Ok(Mode::AcceptEdits),
            "bypassPermissions" => Ok(Mode::BypassPermissions),
            "yolo" | "YOLO" => Ok(Mode::Yolo),
            other => anyhow::bail!(
                "未知权限模式: {other}(可选: ask / acceptEdits / bypassPermissions / yolo)"
            ),
        }
    }
}

/// 一次权限判定结果
pub enum Decision {
    Allow,
    /// 无头模式下无法交互确认,拒绝并给出提示
    Deny(String),
}

/// 命令风险判定(启发式)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandRisk {
    /// 没解析出危险点:正常放行(是否再确认由权限档位决定)
    Safe,
    /// 目标无法判定(变量未知、通配符越界可能、间接调用),或属于需点头的中风险
    Uncertain { reason: String, detail: String },
    /// 目标解析出来确实高危(根/家目录/工作目录上级/块设备)
    Blocked { reason: String, target: String },
}

impl CommandRisk {
    fn rank(&self) -> u8 {
        match self {
            CommandRisk::Safe => 0,
            CommandRisk::Uncertain { .. } => 1,
            CommandRisk::Blocked { .. } => 2,
        }
    }
}

/// 全局权限管理器:只管档位(这个档位是否需要确认),风险判定见 `inspect_command`。
/// 交互模式下的"高危要单独点头"由 session 层统一把关(见 session::check_permission)。
#[derive(Debug, Clone)]
pub struct Permission {
    pub mode: Mode,
}

impl Permission {
    pub fn new(mode: Mode) -> Self {
        Self { mode }
    }

    /// 文件写入类操作(read_file / write_file / edit 等改写文件)
    pub fn check_file_write(&self) -> Decision {
        match self.mode {
            Mode::Ask => Decision::Deny(
                "ask 模式下写文件要确认,无头模式直接拒绝。要自动改文件就加 --permission acceptEdits"
                    .into(),
            ),
            _ => Decision::Allow,
        }
    }

    /// 终端命令执行(只管档位;YOLO 一切放行)
    pub fn check_command(&self, _command: &str) -> Decision {
        match self.mode {
            Mode::Ask | Mode::AcceptEdits => Decision::Deny(
                "该模式下跑命令要确认,无头模式直接拒绝。要自动执行就加 --permission bypassPermissions"
                    .into(),
            ),
            Mode::BypassPermissions | Mode::Yolo => Decision::Allow,
        }
    }
}

// ---------------------------------------------------------------- 命令判定

/// 前缀命令:`sudo rm -rf /` 要看的是 `rm`
const PREFIX_COMMANDS: &[&str] = &[
    "sudo", "doas", "command", "builtin", "exec", "nohup", "setsid", "time", "nice", "ionice",
    "stdbuf", "env", "timeout",
];

/// 会删东西的命令名(xargs / find -exec 用)
const DESTRUCTIVE_NAMES: &[&str] = &["rm", "shred", "unlink", "rmdir"];

/// shell 解释器:带 -c 时递归判定脚本文本
const SHELL_NAMES: &[&str] = &["sh", "bash", "dash", "zsh", "ksh", "fish", "ash"];

/// 块设备路径前缀(命中即视为块设备)
const DEVICE_PREFIXES: &[&str] = &[
    "/dev/sd", "/dev/hd", "/dev/vd", "/dev/xvd", "/dev/nvme", "/dev/mmcblk", "/dev/loop",
    "/dev/mapper/", "/dev/dm-", "/dev/disk", "/dev/rdisk", "/dev/fd0", "/dev/ram",
    "\\\\.\\physicaldrive",
];

/// 判定一条命令的风险(相对 cwd 解析目标路径)
pub fn inspect_command(cmd: &str, cwd: &Path) -> CommandRisk {
    inspect_inner(cmd, cwd, home_dir().as_deref(), 0)
}

fn home_dir() -> Option<PathBuf> {
    dirs::home_dir()
}

fn inspect_inner(cmd: &str, cwd: &Path, home: Option<&Path>, depth: u8) -> CommandRisk {
    if depth > 3 {
        return CommandRisk::Safe;
    }
    let segments = split_segments(cmd);
    if segments.is_empty() {
        return CommandRisk::Safe;
    }
    let vars = collect_assignments(&segments, cwd, home);

    let mut worst = CommandRisk::Safe;
    for seg in &segments {
        let argv: Vec<String> = seg.iter().map(|w| expand_word(w, &vars)).collect();
        let risk = judge_segment(&argv, cwd, home, depth);
        if risk.rank() > worst.rank() {
            worst = risk;
        }
        if worst.rank() == 2 {
            break;
        }
    }
    worst
}

/// 词法切分:按 `;` `&&` `||` `|` `&` 换行 切成简单命令段,每段是词数组。
/// 支持单/双引号与反斜杠转义,`>`/`<` 作为独立 token 保留(重定向目标要看)。
/// 不做变量展开,也不解析 `$( )`(整体留在词里,后面按"含变量"处理)。
fn split_segments(cmd: &str) -> Vec<Vec<String>> {
    let mut segments: Vec<Vec<String>> = Vec::new();
    let mut cur: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut chars = cmd.chars().peekable();

    macro_rules! flush_word {
        () => {
            if !word.is_empty() {
                cur.push(std::mem::take(&mut word));
            }
        };
    }
    macro_rules! flush_segment {
        () => {{
            flush_word!();
            if !cur.is_empty() {
                segments.push(std::mem::take(&mut cur));
            }
        }};
    }

    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else if q == '"' && c == '\\' {
                    // 双引号内反斜杠转义下一个字符
                    if let Some(n) = chars.next() {
                        word.push(n);
                    }
                } else {
                    word.push(c);
                }
            }
            None => match c {
                '\'' | '"' => quote = Some(c),
                '\\' => {
                    if let Some(n) = chars.next() {
                        word.push(n);
                    }
                }
                ' ' | '\t' | '\r' => flush_word!(),
                '\n' | ';' => flush_segment!(),
                '|' => {
                    if chars.peek() == Some(&'|') {
                        chars.next();
                    }
                    flush_segment!();
                }
                '&' => {
                    if chars.peek() == Some(&'&') {
                        chars.next();
                    }
                    flush_segment!();
                }
                '>' | '<' => {
                    // 重定向操作符独立成词(`>>` `2>` `&>` `<<` …),目标看下一个词
                    flush_word!();
                    let mut op = String::from(c);
                    while let Some(&n) = chars.peek() {
                        if n == c || (n == '|' && c == '>') {
                            op.push(n);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    cur.push(op);
                }
                _ => word.push(c),
            },
        }
    }
    flush_segment!();
    segments
}

/// 收集同一条命令串内的字面变量赋值(`T=/; rm -rf $T` 这种),值含 `$` 或反引号则忽略。
/// 同时预置 `HOME` 与 `PWD`。
fn collect_assignments(
    segments: &[Vec<String>],
    cwd: &Path,
    home: Option<&Path>,
) -> HashMap<String, String> {
    let mut vars = HashMap::new();
    if let Some(h) = home {
        vars.insert("HOME".to_string(), h.to_string_lossy().to_string());
    }
    vars.insert("PWD".to_string(), cwd.to_string_lossy().to_string());
    for seg in segments {
        for w in seg {
            if let Some((name, val)) = w.split_once('=') {
                if !is_var_name(name) || val.contains('$') || val.contains('`') {
                    continue;
                }
                vars.insert(name.to_string(), val.to_string());
            }
        }
    }
    vars
}

fn is_var_name(s: &str) -> bool {
    let mut it = s.chars();
    match it.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    it.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// 展开 `$VAR` / `${VAR}`;未知变量原样保留(后续按"含未解析变量"处理)
fn expand_word(w: &str, vars: &HashMap<String, String>) -> String {
    if !w.contains('$') {
        return w.to_string();
    }
    let mut out = String::with_capacity(w.len());
    let mut chars = w.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'{') {
            chars.next();
            let mut name = String::new();
            for c2 in chars.by_ref() {
                if c2 == '}' {
                    break;
                }
                name.push(c2);
            }
            match vars.get(&name) {
                Some(v) => out.push_str(v),
                None => {
                    out.push_str("${");
                    out.push_str(&name);
                    out.push('}');
                }
            }
        } else {
            let mut name = String::new();
            while let Some(&c2) = chars.peek() {
                if c2.is_ascii_alphanumeric() || c2 == '_' {
                    name.push(c2);
                    chars.next();
                } else {
                    break;
                }
            }
            if name.is_empty() {
                out.push('$'); // $? $$ $1 之类,留在串里
                continue;
            }
            match vars.get(&name) {
                Some(v) => out.push_str(v),
                None => {
                    out.push('$');
                    out.push_str(&name);
                }
            }
        }
    }
    out
}

/// 一段简单命令的判定
fn judge_segment(argv: &[String], cwd: &Path, home: Option<&Path>, depth: u8) -> CommandRisk {
    // 重定向目标:`echo x > /dev/sda`
    for (i, t) in argv.iter().enumerate() {
        if is_redirect_op(t) {
            if let Some(tgt) = argv.get(i + 1) {
                if let TargetClass::Dangerous(why, shown) = classify_target(tgt, cwd, home) {
                    return CommandRisk::Blocked {
                        reason: format!("重定向写入{why}"),
                        target: shown,
                    };
                }
            }
        }
    }

    let argv = strip_prefixes(argv);
    let Some(first) = argv.first() else {
        return CommandRisk::Safe;
    };
    let cmd = basename(first);
    let args = &argv[1..];

    // 解释器 -c:递归看脚本文本
    if SHELL_NAMES.contains(&cmd) {
        if let Some(script) = script_arg(args) {
            return inspect_inner(&script, cwd, home, depth + 1);
        }
        return CommandRisk::Safe;
    }

    // xargs rm …:目标来自标准输入,无法判定
    if cmd == "xargs" {
        if args.iter().any(|a| DESTRUCTIVE_NAMES.contains(&basename(a))) {
            return CommandRisk::Uncertain {
                reason: "无法确定 xargs 的删除目标".into(),
                detail: "待删路径来自标准输入,判定时看不到".into(),
            };
        }
        return CommandRisk::Safe;
    }

    match cmd {
        "rm" => {
            let recursive = has_short_flag(args, 'r')
                || has_short_flag(args, 'R')
                || has_long_flag(args, "recursive");
            if !recursive {
                return CommandRisk::Safe;
            }
            worst_of_targets(&operands(args), cwd, home, "递归删除")
        }
        "find" => {
            let delete = args.iter().any(|a| a == "-delete");
            let exec_rm = args.windows(2).any(|w| {
                (w[0] == "-exec" || w[0] == "-execdir") && DESTRUCTIVE_NAMES.contains(&basename(&w[1]))
            });
            if !delete && !exec_rm {
                return CommandRisk::Safe;
            }
            // find 的第一个非选项词是起始目录
            match args.iter().find(|t| !t.starts_with('-')) {
                Some(start) => worst_of_targets(&[start.as_str()], cwd, home, "删除"),
                None => CommandRisk::Safe,
            }
        }
        "dd" => {
            for a in args {
                if let Some(v) = a.strip_prefix("of=") {
                    match classify_target(v, cwd, home) {
                        TargetClass::Dangerous(why, shown) => {
                            return CommandRisk::Blocked {
                                reason: format!("dd 写入{why}"),
                                target: shown,
                            }
                        }
                        TargetClass::Uncertain(d) => {
                            return CommandRisk::Uncertain {
                                reason: "无法确定 dd 的写入目标".into(),
                                detail: d,
                            }
                        }
                        TargetClass::Safe => {}
                    }
                }
            }
            CommandRisk::Safe
        }
        // mkfs.ext4 / mkfs -t ext4 …:参数里出现块设备/高危路径
        c if c.starts_with("mkfs") => worst_of_targets(&operands(args), cwd, home, "格式化"),
        // 分区工具:只读列举(-l)放行,否则看目标
        "fdisk" | "sfdisk" | "parted" | "sgdisk" | "wipefs" => {
            if has_short_flag(args, 'l') || has_long_flag(args, "list") {
                return CommandRisk::Safe;
            }
            worst_of_targets(&operands(args), cwd, home, "分区/擦除")
        }
        "chmod" | "chown" | "chgrp" | "setfacl" => {
            if has_short_flag(args, 'R') || has_long_flag(args, "recursive") {
                worst_of_targets(&operands(args), cwd, home, "递归改权限")
            } else {
                CommandRisk::Safe
            }
        }
        "truncate" | "shred" => worst_of_targets(&operands(args), cwd, home, "清空/粉碎"),
        "git" => {
            if !args.iter().any(|a| a == "push") {
                return CommandRisk::Safe;
            }
            let lease = args
                .iter()
                .any(|a| a == "--force-with-lease" || a.starts_with("--force-with-lease="));
            if !lease && (has_long_flag(args, "force") || has_short_flag(args, 'f')) {
                return CommandRisk::Blocked {
                    reason: "git 强制推送(--force),可能覆盖远端历史".into(),
                    target: "远端分支".into(),
                };
            }
            CommandRisk::Safe
        }
        "shutdown" | "reboot" | "poweroff" | "halt" | "init" => CommandRisk::Uncertain {
            reason: "系统级操作,请确认".into(),
            detail: format!("{cmd} 会重启/关机,影响本机所有工作"),
        },
        _ => CommandRisk::Safe,
    }
}

/// 逐个目标判定,取最严重的一个
fn worst_of_targets(targets: &[&str], cwd: &Path, home: Option<&Path>, verb: &str) -> CommandRisk {
    let mut worst = CommandRisk::Safe;
    for t in targets {
        match classify_target(t, cwd, home) {
            TargetClass::Dangerous(why, shown) => {
                return CommandRisk::Blocked {
                    reason: format!("检测到{verb}{why}"),
                    target: shown,
                }
            }
            TargetClass::Uncertain(d) => {
                if worst.rank() < 1 {
                    worst = CommandRisk::Uncertain {
                        reason: format!("{verb}的目标无法确定"),
                        detail: d,
                    };
                }
            }
            TargetClass::Safe => {}
        }
    }
    worst
}

/// 去掉 sudo/env/timeout 之类前缀与前置变量赋值(`FOO=1 rm -rf /`),拿到真正的命令
fn strip_prefixes(mut argv: &[String]) -> &[String] {
    let is_assignment = |t: &str| t.split_once('=').map(|(n, _)| is_var_name(n)).unwrap_or(false);
    while let Some(t) = argv.first() {
        if is_assignment(t) {
            argv = &argv[1..];
        } else {
            break;
        }
    }
    loop {
        let Some(first) = argv.first() else { return argv };
        let cmd = basename(first);
        if !PREFIX_COMMANDS.contains(&cmd) {
            return argv;
        }
        let prefix = cmd;
        argv = &argv[1..];
        while let Some(t) = argv.first() {
            let takes_value = prefix_flag_takes_value(prefix, t);
            let is_flag_or_assign = (t.starts_with('-') && t.len() > 1) || t.contains('=');
            let is_duration = t.chars().all(|c| c.is_ascii_digit() || c == '.' || c == 's' || c == 'm' || c == 'h');
            if is_flag_or_assign || is_duration {
                argv = &argv[1..];
                if takes_value {
                    if let Some(_v) = argv.first() {
                        argv = &argv[1..];
                    }
                }
            } else {
                break;
            }
        }
    }
}

/// 前缀命令里"带值"的选项(启发式清单,漏了只会导致判不出,不会误判)
fn prefix_flag_takes_value(prefix: &str, flag: &str) -> bool {
    match prefix {
        "sudo" | "doas" => matches!(flag, "-u" | "-g" | "-h" | "-p" | "-C" | "-U" | "-r" | "-t"),
        "nice" | "ionice" => matches!(flag, "-n" | "-c" | "-p" | "-u"),
        "timeout" => matches!(flag, "-s" | "-k"),
        "env" | "stdbuf" => matches!(flag, "-u" | "-C" | "-o" | "-e"),
        _ => false,
    }
}

fn basename(s: &str) -> &str {
    let name = s.rsplit(['/', '\\']).next().unwrap_or(s);
    name.strip_suffix(".exe").unwrap_or(name)
}

fn is_redirect_op(t: &str) -> bool {
    !t.is_empty()
        && (t.contains('>') || t.contains('<'))
        && t.chars().all(|c| matches!(c, '>' | '<' | '|' | '&' | '0'..='9'))
}

fn has_short_flag(args: &[String], ch: char) -> bool {
    args.iter()
        .any(|a| a.starts_with('-') && !a.starts_with("--") && a.len() > 1 && a[1..].contains(ch))
}

fn has_long_flag(args: &[String], name: &str) -> bool {
    args.iter()
        .any(|a| a == &format!("--{name}") || a.starts_with(&format!("--{name}=")))
}

/// 非选项参数(跳过 `-x` 与 `--` 之后的处理)
fn operands(args: &[String]) -> Vec<&str> {
    let mut out = Vec::new();
    let mut no_more_flags = false;
    for a in args {
        if !no_more_flags && a == "--" {
            no_more_flags = true;
            continue;
        }
        if !no_more_flags && a.starts_with('-') && a.len() > 1 {
            continue;
        }
        out.push(a.as_str());
    }
    out
}

/// 解释器 `-c` 后面的脚本文本
fn script_arg(args: &[String]) -> Option<String> {
    for (i, a) in args.iter().enumerate() {
        if a.starts_with('-') && !a.starts_with("--") && a[1..].contains('c') {
            return args.get(i + 1).cloned();
        }
    }
    None
}

// ---------------------------------------------------------------- 目标路径

enum TargetClass {
    Safe,
    /// (原因, 规范化后的目标)
    Dangerous(String, String),
    Uncertain(String),
}

/// 判定一个目标词是否高危:先规范化(展开 `~`、按 cwd 绝对化、折叠 `.`/`..`),再比较。
/// 含未解析变量/命令替换 → 无法判定;含通配符 → 只看通配符所在目录(通配符不会
/// 匹配 `.`/`..`,所以安全目录下的通配符爬不出去),但出现 `..` 时无法确定,保守处理。
fn classify_target(raw: &str, cwd: &Path, home: Option<&Path>) -> TargetClass {
    let t = raw.trim();
    if t.is_empty() {
        return TargetClass::Safe;
    }
    if t.contains('$') || t.contains('`') {
        return TargetClass::Uncertain("含未解析的变量或命令替换".into());
    }
    if t.starts_with('~') && !(t.len() == 1 || t.starts_with("~/")) {
        return TargetClass::Uncertain("含 ~user 形式,无法确定家目录".into());
    }

    let has_glob = t.contains(['*', '?', '[']);
    if has_glob {
        if t.split('/').any(|c| c == "..") {
            return TargetClass::Uncertain("通配符与 .. 混用,无法确定最终目标".into());
        }
        let base = t.split(['*', '?', '[']).next().unwrap_or("");
        if let Some(p) = resolve_path(base, cwd, home) {
            let dir = if base.is_empty() || base.ends_with('/') {
                p.clone()
            } else {
                p.parent().map(|x| x.to_path_buf()).unwrap_or_else(|| p.clone())
            };
            if let Some(why) = dangerous_why(&dir, cwd, home) {
                return TargetClass::Dangerous(why, dir.display().to_string());
            }
        }
        // 通配符只在安全目录内展开,爬不出去
        return TargetClass::Safe;
    }

    match resolve_path(t, cwd, home) {
        Some(p) => match dangerous_why(&p, cwd, home) {
            Some(why) => TargetClass::Dangerous(why, p.display().to_string()),
            None => TargetClass::Safe,
        },
        None => TargetClass::Uncertain("无法解析该路径".into()),
    }
}

/// 高危目标:根目录 / 家目录 / 块设备 / 当前工作目录或其上级
fn dangerous_why(p: &Path, cwd: &Path, home: Option<&Path>) -> Option<String> {
    if p == Path::new("/") {
        return Some("根目录 /".into());
    }
    if let Some(h) = home {
        if p == h {
            return Some("家目录 ~".into());
        }
    }
    if is_device_path(p) {
        return Some("块设备".into());
    }
    if cwd.starts_with(p) {
        return Some("当前工作目录或其上级目录".into());
    }
    None
}

fn is_device_path(p: &Path) -> bool {
    let s = p.to_string_lossy().to_lowercase();
    DEVICE_PREFIXES.iter().any(|pre| s.starts_with(pre))
}

/// 展开 `~`,相对路径按 cwd 绝对化,再做纯词法折叠(不碰文件系统)
fn resolve_path(raw: &str, cwd: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let expanded = if let Some(rest) = raw.strip_prefix("~/") {
        format!("{}/{}", home?.to_string_lossy(), rest)
    } else if raw == "~" {
        home?.to_string_lossy().to_string()
    } else {
        raw.to_string()
    };
    let p = if expanded.starts_with('/') || expanded.starts_with('\\') {
        PathBuf::from(&expanded)
    } else {
        cwd.join(&expanded)
    };
    Some(normalize_lexically(&p))
}

fn normalize_lexically(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::RootDir => out.push("/"),
            Component::Prefix(pre) => out.push(pre.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(x) => out.push(x),
        }
    }
    if out.as_os_str().is_empty() {
        out.push("/");
    }
    out
}

/// 快照到 undo 目录,事后可回滚;返回快照路径,文件不存在(新建)返回 None
pub fn snapshot_file(session_dir: &Path, file: &Path) -> anyhow::Result<Option<std::path::PathBuf>> {
    if !file.exists() {
        return Ok(None);
    }
    let meta = std::fs::metadata(file)?;
    if meta.is_dir() {
        return Ok(None);
    }
    let target = session_dir.join(file.strip_prefix("/").unwrap_or(file));
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(file, &target)?;
    Ok(Some(target))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CWD: &str = "/home/u/proj";
    const HOME: &str = "/home/u";

    fn risk(cmd: &str) -> CommandRisk {
        inspect_inner(cmd, Path::new(CWD), Some(Path::new(HOME)), 0)
    }
    fn blocked(cmd: &str) -> bool {
        matches!(risk(cmd), CommandRisk::Blocked { .. })
    }
    fn uncertain(cmd: &str) -> bool {
        matches!(risk(cmd), CommandRisk::Uncertain { .. })
    }
    fn safe(cmd: &str) -> bool {
        matches!(risk(cmd), CommandRisk::Safe)
    }

    #[test]
    fn blocked_targets() {
        assert!(blocked("rm -rf /"));
        assert!(blocked("rm -rf ~"));
        assert!(blocked("rm -rf /home/u"), "家目录本身");
        assert!(blocked("rm -rf .."), "父目录 = 家目录");
        assert!(blocked("rm -rf ."), "工作目录本身");
        assert!(blocked("rm -rf .*"), "隐藏文件通配符覆盖 cwd");
        assert!(blocked("rm -rf /*"), "根下通配符");
        assert!(blocked("rm -rf /tmp/../.."), ".. 折叠后回到根");
    }

    #[test]
    fn whitespace_and_flag_order_do_not_matter() {
        // 用户最初报的绕过:中间多一个空格
        assert!(blocked("rm  -rf /"));
        assert!(blocked("rm\t-rf\t/"));
        assert!(blocked("rm -r -f /"));
        assert!(blocked("rm -f -r /"));
        assert!(blocked("rm --recursive --force /"));
        assert!(blocked("rm -rfv /"));
        assert!(blocked("rm -rf \"/\""));
        assert!(blocked("rm -rf '/'"));
        assert!(blocked("sudo rm -rf /"));
        assert!(blocked("sudo -u root rm -rf /"));
        assert!(blocked("env FOO=1 rm -rf /"));
        assert!(blocked("FOO=1 rm -rf /"));
        assert!(blocked("timeout 5 rm -rf /"));
        assert!(blocked("bash -c 'rm -rf /'"));
        assert!(blocked("/bin/rm -rf /"));
    }

    #[test]
    fn literal_assignments_are_followed() {
        // 同一条命令串内的字面赋值可以解析出来(比原计划的 Uncertain 更进一步)
        assert!(blocked("T=/; rm -rf $T"));
        assert!(blocked("T=/ ; rm -rf ${T}"));
        assert!(safe("D=./build; rm -rf $D"));
    }

    #[test]
    fn specific_paths_are_allowed() {
        // 用户最初报的误伤:具体路径不该硬拒
        assert!(safe("rm -rf /tmp/123/"));
        assert!(safe("rm -rf /tmp/123"));
        assert!(safe("rm -rf ~/x"));
        assert!(safe("rm -rf ./build"));
        assert!(safe("rm -rf build target"));
        assert!(safe("rm -rf /tmp/*"), "通配符在安全目录内爬不出去");
    }

    #[test]
    fn other_commands() {
        assert!(blocked("dd if=/dev/zero of=/dev/sda bs=1M"));
        assert!(safe("dd if=backup.img of=./disk.img"));
        assert!(safe("dd if=/dev/zero of=/dev/null"));
        assert!(blocked("mkfs.ext4 /dev/sdb1"));
        assert!(blocked("mkfs -t ext4 /dev/sdb1"));
        assert!(blocked("chmod -R 777 /"));
        assert!(blocked("chown -R u:u /"));
        assert!(safe("chmod -R 755 ./dir"));
        assert!(blocked("find / -delete"));
        assert!(blocked("find . -name '*.log' -delete"));
        assert!(safe("find ./build -delete"));
        assert!(blocked("echo x > /dev/sda"));
        assert!(safe("echo hi > /tmp/x.txt"));
        assert!(safe("fdisk -l"));
        assert!(safe("fdisk -l /dev/sda"), "-l 是只读列举");
        assert!(blocked("fdisk /dev/sda"));
        assert!(blocked("shred /dev/sda"));
    }

    #[test]
    fn git_push() {
        assert!(blocked("git push --force origin main"));
        assert!(blocked("git push -f origin main"));
        assert!(safe("git push origin main"));
        assert!(safe("git push --force-with-lease origin main"));
        assert!(safe("grep -r force ."));
    }

    #[test]
    fn uncertain_cases() {
        assert!(uncertain("rm -rf $UNKNOWN_DIR"));
        assert!(uncertain("rm -rf /tmp/$X/../.."));
        assert!(uncertain("xargs rm -rf"));
        assert!(uncertain("shutdown -h now"));
        assert!(uncertain("reboot"));
    }

    #[test]
    fn quoted_text_is_not_a_command() {
        // 原实现这里会误伤(contains 命中),现在按 argv 判定
        assert!(safe("echo \"rm -rf /\""));
        assert!(safe("grep -rn 'rm -rf /' docs/"));
        assert!(safe("ls -la"));
        assert!(safe("sed -i s/x/y/ README.md"));
    }

    #[test]
    fn worst_segment_wins() {
        assert!(blocked("echo start && rm -rf /"));
        assert!(blocked("cd /tmp | rm -rf /"));
        assert!(safe("echo a; ls; pwd"));
    }

    #[test]
    fn mode_parsing() {
        assert_eq!(Mode::parse("ask").unwrap(), Mode::Ask);
        assert!(Mode::parse("nope").is_err());
    }

    #[test]
    fn ask_mode_denies_writes_and_commands() {
        let p = Permission::new(Mode::Ask);
        assert!(matches!(p.check_file_write(), Decision::Deny(_)));
        assert!(matches!(p.check_command("echo hi"), Decision::Deny(_)));
    }

    #[test]
    fn bypass_allows_but_yolo_is_wider_still() {
        let p = Permission::new(Mode::BypassPermissions);
        assert!(matches!(p.check_file_write(), Decision::Allow));
        assert!(matches!(p.check_command("echo hi"), Decision::Allow));
        let y = Permission::new(Mode::Yolo);
        assert!(matches!(y.check_command("rm -rf /"), Decision::Allow));
        assert_eq!(Mode::parse("YOLO").unwrap(), Mode::Yolo);
    }
}
