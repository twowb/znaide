//! 命令语义:有哪些命令、帮助文本、技能清单,以及 slash 命令的**分发**。
//!
//! 放在宿主层是为了让两个客户端"说同一套话":命令集合、每句提示、每个动作的含义
//! 都只有一份。前端只做两件事:
//!
//! - 把 [`CommandResult::lines`] 渲染出来(`Line::Notice` 是系统提示行,
//!   `Line::Output` 是等宽输出块);
//! - 按 [`Outcome`] 决定后续动作(开确认框 / 开会话窗口 / 退出 / 起更新任务 / 发回合)。
//!
//! 需要界面的分支(清空确认、会话窗口、更新任务)以 `Outcome` 回给前端,**不在这里弹**。

use std::path::{Path, PathBuf};

use tokio::sync::mpsc::UnboundedSender;
use znaide_core::session::list_history_sessions_detailed;

use crate::agent::AgentCmd;

/// 输出行的类别(前端据此选渲染样式)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Line {
    /// 系统提示行(消息流里的一条通知)
    Notice,
    /// 等宽输出块(帮助、列表、清单)
    Output,
}

/// 命令要求的后续动作。
#[derive(Debug)]
pub enum Outcome {
    /// 内部处理完毕
    Handled,
    /// 破坏性操作:前端弹确认框,y 之后发 `AgentCmd::ClearContext`
    NeedConfirmClear { title: String, body: String },
    /// `/resume` 无参:打开会话管理窗口
    OpenSessions,
    /// `/quit` / `/exit`
    Quit,
    /// `/compact`
    Compact,
    /// `/update`:前端起后台任务(下载可能很久,进度走自己的通道)
    RunUpdate,
    /// 作为新回合的普通指令(技能无入口时,正文即指令)
    Task(String),
    /// 交给 agent 执行技能(可能有入口脚本,权限与工具事件在 agent 内统一处理)
    RunSkill { name: String, args: String },
}

/// 一次命令处理的结果。
pub struct CommandResult {
    pub outcome: Outcome,
    pub lines: Vec<(Line, String)>,
}

impl CommandResult {
    fn new() -> Self {
        Self {
            outcome: Outcome::Handled,
            lines: Vec::new(),
        }
    }

    fn notice(&mut self, text: impl Into<String>) {
        self.lines.push((Line::Notice, text.into()));
    }

    fn output(&mut self, text: impl Into<String>) {
        self.lines.push((Line::Output, text.into()));
    }
}

/// 处理一条 slash 命令。不认识的名字按"技能手动触发"处理(旧自定义命令同款行为)。
pub fn run(raw: &str, cmd_tx: &UnboundedSender<AgentCmd>, cwd: &Path) -> CommandResult {
    let mut r = CommandResult::new();
    let parts: Vec<&str> = raw.split_whitespace().collect();
    let name = parts.first().copied().unwrap_or("");
    match name {
        "/help" => r.output(HELP_TEXT),
        "/clear" => {
            // 破坏性操作:先弹确认,y 才真正清空
            r.outcome = Outcome::NeedConfirmClear {
                title: "清空会话?".into(),
                body: "将清空本会话全部上下文(模型记忆)与历史文件(jsonl),不可恢复。\n清空后继续对话不再带此前任何内容,重要内容先自己保存。".into(),
            };
        }
        "/skills" => {
            let txt = skills_text(cwd);
            if txt.is_empty() {
                r.output("暂无已安装技能。\n技能目录:~/.znaide/skills/ 用户级, .znaide/skills/ 项目级");
            } else {
                r.output(txt);
            }
        }
        "/undo" => {
            if parts.len() >= 2 {
                if let Ok(idx) = parts[1].parse::<usize>() {
                    match znaide_core::undo::rollback_by_index(idx) {
                        Ok(snap) => r.notice(format!(
                            "已回滚 #{idx}: {} ← 快照 {}",
                            snap.orig, snap.ts_ms
                        )),
                        Err(e) => r.output(format!("回滚失败: {e}")),
                    }
                } else {
                    r.output("用法:/undo(列表) 或 /undo <序号>(回滚)");
                }
                return r;
            }
            let snaps = znaide_core::undo::list();
            if snaps.is_empty() {
                r.output("暂无 undo 快照(还没有文件被修改过)");
                return r;
            }
            let mut out = format!("undo 快照(最新在前,共 {}):\n", snaps.len());
            for (i, s) in snaps.iter().take(20).enumerate() {
                let t = s.ts_ms as f64 / 1000.0;
                let ext = if s.is_external() { " ·外部" } else { "" };
                out.push_str(&format!(
                    "  {}. {} — {} ({}){}\n",
                    i + 1,
                    s.action,
                    s.orig,
                    fmt_ago(t),
                    ext
                ));
            }
            if snaps.len() > 20 {
                out.push_str(&format!("…(共 {} 条,最新 20 条)\n", snaps.len()));
            }
            r.output(out);
        }
        "/resume" => {
            // 无参:打开全屏会话管理窗口(会话 / 记忆两个页签)
            if parts.len() == 1 {
                r.outcome = Outcome::OpenSessions;
                return r;
            }
            // 带参数:恢复 / 删除(兼容旧行为,脚本友好)
            let history = list_history_sessions_detailed();
            if history.is_empty() {
                r.output("暂无历史会话(~/.znaide/sessions/ 为空)");
                return r;
            }
            let paths: Vec<PathBuf> = history.iter().map(|h| h.path.clone()).collect();
            if parts[1] == "del" {
                // /resume del <序号|片段> 删除历史
                if parts.len() < 3 {
                    r.output("用法:/resume del <序号|文件名片段>");
                    return r;
                }
                let target = parts[2];
                match match_session(&paths, target) {
                    Some(path) => match znaide_core::session::remove_session(&path) {
                        Ok(()) => r.notice(format!("🗑 已删除历史会话: {}", path.display())),
                        Err(e) => r.output(format!("删除失败: {e}")),
                    },
                    None => r.output(format!("未找到匹配「{target}」的历史会话")),
                }
                return r;
            }
            // 传入序号或文件路径 → 恢复
            let target = parts[1];
            match match_session(&paths, target) {
                Some(path) => {
                    let _ = cmd_tx.send(AgentCmd::LoadHistory(path));
                    r.notice("正在恢复历史会话…");
                }
                None => r.output(format!("未找到匹配「{target}」的历史会话")),
            }
        }
        "/compact" => {
            r.notice("▶ 请求压缩上下文…");
            r.outcome = Outcome::Compact;
        }
        "/quit" | "/exit" => {
            r.notice("正在退出…");
            r.outcome = Outcome::Quit;
        }
        "/update" => {
            // 后台执行(下载可能要一阵),进度/结果经前端自己的更新通道回显
            r.notice("正在检查更新…(Gitee 优先,不通自动切 GitHub)");
            r.outcome = Outcome::RunUpdate;
        }
        "/persona" => {
            let avail = znaide_core::persona::list();
            let current = znaide_core::config::Config::load()
                .ok()
                .and_then(|c| c.persona)
                .unwrap_or_default();
            let cur_label = if current.is_empty() {
                "无(默认助手)".to_string()
            } else {
                format!("「{current}」")
            };
            if parts.len() >= 2 {
                let target = parts[1];
                let known = target == "无" || avail.iter().any(|n| n == target);
                if !known {
                    let hint = if avail.is_empty() {
                        "无".to_string()
                    } else {
                        format!("{}、无", avail.join("、"))
                    };
                    r.output(format!("没有这个人格:「{target}」。可用:{hint}"));
                    return r;
                }
                let _ = cmd_tx.send(AgentCmd::SetPersona(target.to_string()));
                r.notice(format!(
                    "正在切换人格「{target}」…(重建系统提示,之后所有对话都带着它)"
                ));
                return r;
            }
            let mut out = format!("人格设置(当前:{cur_label},全局生效)\n\n可用人格:\n");
            for n in &avail {
                out.push_str(&format!("  /persona {n}\n"));
            }
            out.push_str("  /persona 无   关闭人格,恢复默认助手\n\n自定义:往 ~/.znaide/personas/ 放一个 <名字>.md(正文即人设说明),下次启动即可用 /persona 选它");
            r.output(out);
        }
        _ => {
            // 不是内置命令:当技能 /名字 手动触发(技能目录 ~/.znaide/skills/ 或 .znaide/skills/)
            let cmd_name = name.trim_start_matches('/');
            let set = znaide_core::skills::scan(cwd);
            match znaide_core::skills::find(&set.skills, cmd_name) {
                Some(skill) => {
                    let args = parts[1..].join(" ");
                    r.notice(format!(
                        "▶ 技能「{}」已触发: {}({})",
                        skill.name,
                        skill.description,
                        skill.source.label()
                    ));
                    if skill.entry.is_some() {
                        r.notice("该技能声明了入口脚本,执行时可能需要你确认。");
                        r.outcome = Outcome::RunSkill {
                            name: skill.name.clone(),
                            args,
                        };
                        return r;
                    }
                    // 无入口:正文即本回合指令(旧自定义命令同款行为)
                    r.outcome = Outcome::Task(skill.render(&args));
                    return r;
                }
                None => {
                    let hint = if set.skills.is_empty() {
                        String::new()
                    } else {
                        "\n输入 /skills 可查看已安装技能".to_string()
                    };
                    r.output(format!("未知命令: {name}\n输入 /help 查看内置命令{hint}"));
                }
            }
        }
    }
    r
}

/// 按序号(从 1 开始)或文件名片段匹配历史会话。
fn match_session(sessions: &[PathBuf], target: &str) -> Option<PathBuf> {
    if let Ok(idx) = target.parse::<usize>() {
        return sessions.get(idx.saturating_sub(1)).cloned();
    }
    sessions
        .iter()
        .find(|p| {
            p.to_string_lossy().contains(target)
                || p.file_stem()
                    .map(|s| s.to_string_lossy().contains(target))
                    .unwrap_or(false)
        })
        .cloned()
}

/// 把 epoch 秒格式化成"多久以前"(undo 列表用)。
pub fn fmt_ago(epoch_secs: f64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let diff = now - epoch_secs;
    if diff < 60.0 {
        format!("{:.0}秒前", diff)
    } else if diff < 3600.0 {
        format!("{:.0}分钟前", diff / 60.0)
    } else if diff < 86400.0 {
        format!("{:.1}小时前", diff / 3600.0)
    } else {
        format!("{:.0}天前", diff / 86400.0)
    }
}

pub const HELP_TEXT: &str = "可用命令:
  /help                    显示本帮助
  /skills                  列出已安装技能
  /config                  打开配置面板(随时修改 provider/模型/端点/key/轮数上限,立即生效)
  /undo                    列出 undo 快照; /undo <序号> 回滚
  /resume                  打开会话管理窗口(会话/记忆页签,多选批量删、备注、恢复); /resume <序号|片段> 恢复; /resume del <序号|片段> 删除
  /clear                   清空当前会话上下文与历史文件(需确认)
  /compact                 压缩上下文:旧对话收敛为摘要,腾出空间继续对话
  /update                  检查并更新到 GitHub 最新版本
  /persona                 人格设置:/persona 列表, /persona <名字> 切换, /persona 无 关闭
  /技能名                  触发已安装技能(列表见 /skills)

输入技巧:
  @文件 或 @目录          引用本地文件/目录(自动注入内容)
  Enter                   发送
  Shift+Enter 换行        若终端不支持,可用 Alt+Enter 或 Ctrl+J
  Shift+Tab               循环切换权限模式(询问 → 编辑放行 → 全自动 → 超级)
  Shift+拖动              选择并复制文字(鼠标已用于滚轮/滚动条,按住 Shift 可拖选)
  Esc                     中断生成 / 取消
  Ctrl+C                  退出

权限模式说明(Shift+Tab 循环切换;输入框与消息区边框颜色随模式变化):
  询问       写文件、执行命令前均需你确认(默认,边框绿色)
  编辑放行   文件修改自动放行,命令执行仍确认(边框天蓝)
  全自动     全部自动执行,高危命令仍要人工确认(边框紫色)
  超级 YOLO  一切放行、无任何问询,高危判定也跳过——高度危险,后果自负(边框红色)
";

/// 列出已安装技能(供 `/skills` 展示);附带扫描告警,便于发现坏 SKILL.md。
/// 技能为空且无告警时返回空串(前端据此什么都不显示)。
pub fn skills_text(cwd: &Path) -> String {
    let set = znaide_core::skills::scan(cwd);
    if set.skills.is_empty() && set.warnings.is_empty() {
        return String::new();
    }
    let mut out = String::from("已安装技能(直接输入 /名字 触发;模型也可自主调用):\n");
    for s in &set.skills {
        let flag = if s.disable_model_invocation {
            " [仅手动]"
        } else {
            ""
        };
        out.push_str(&format!(
            "  /{} — {}({}){}\n",
            s.name,
            s.description,
            s.source.label(),
            flag
        ));
    }
    if set.skills.is_empty() {
        out.push_str("  (暂无可用技能)\n");
    }
    for w in &set.warnings {
        out.push_str(&format!("  ⚠ {w}\n"));
    }
    out.push_str("(技能目录:~/.znaide/skills/ 用户级, .znaide/skills/ 项目级)");
    out
}
