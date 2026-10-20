//! 命令语义:有哪些命令、帮助文本、技能清单文案。
//!
//! 放在宿主层是为了让两个客户端**说同一套话**:命令集合与说明只有一份,
//! 前端只负责"怎么显示"(TUI 塞进消息流,GUI 渲染成侧栏页)。
//!
//! 分发(`/undo`、`/compact`、`/resume`…)还没搬过来 —— 它和界面的
//! "确认框/会话窗口"耦合着,按后续独立一步处理。

use std::path::Path;

use znaide_core::skills::scan;

/// 全部命令与输入技巧(帮助页原文)。
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
    let set = scan(cwd);
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
