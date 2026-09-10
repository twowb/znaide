pub mod file;
pub mod memory;
pub mod search;
pub mod shell;
pub mod web;

use crate::permissions::{Decision, Permission};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

/// 工具执行上下文
pub struct ToolContext<'a> {
    pub cwd: &'a Path,
    pub permission: &'a Permission,
    /// 当前会话 id(undo 快照按会话聚合)
    pub session_id: &'a str,
    /// 本轮取消令牌:长耗时工具(命令执行等)在用户 Esc 中断时提前终止
    pub cancel: Option<CancellationToken>,
    /// 工具运行期事件通道(shell 静默预警等实时信号;无头模式为 None)
    pub events: Option<&'a tokio::sync::mpsc::UnboundedSender<crate::session::SessionEvent>>,
}

/// 工具执行结果:直接回给模型的文本
pub type ToolOutput = String;

/// 工具执行错误:错误文本同样会回给模型,让它自行修正
#[derive(Debug)]
pub struct ToolError(pub String);

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<std::io::Error> for ToolError {
    fn from(e: std::io::Error) -> Self {
        ToolError(format!("I/O 错误: {e}"))
    }
}

impl From<serde_json::Error> for ToolError {
    fn from(e: serde_json::Error) -> Self {
        ToolError(format!("参数解析错误: {e}"))
    }
}

/// 把模型传入的路径参数解析为绝对路径(相对 cwd)
pub fn resolve_path(cwd: &Path, p: &str) -> PathBuf {
    let pb = PathBuf::from(p);
    if pb.is_absolute() {
        pb
    } else {
        cwd.join(pb)
    }
}

/// 工具注册表:名称 -> (描述 + JSON Schema)
pub struct ToolRegistry {
    tools: Vec<(&'static str, &'static str, Value)>,
}

/// 组装 JSON Schema。required 显式声明必填参数——缺了它,部分模型会整段省略
/// 参数(输出空 arguments),导致每个工具都报参数解析错误。
fn schema(props: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": props,
        "required": required
    })
}

impl ToolRegistry {
    pub fn builtin() -> Self {
        let tools = vec![
            (
                "list_directory",
                "列出目录下的条目(文件与子目录名),用于了解目录结构。缺省列出当前工作目录",
                schema(
                    json!({
                        "path": {"type": "string", "description": "要列出的目录路径,相对或绝对;缺省当前工作目录"}
                    }),
                    &[],
                ),
            ),
            (
                "read_file",
                "读取文本文件内容(支持分页)。适合查看文件、代码、配置、日志等",
                schema(
                    json!({
                        "path": {"type": "string", "description": "文件路径,相对或绝对"},
                        "offset": {"type": "integer", "description": "从第几行开始读,从 0 计,缺省从开头"},
                        "limit": {"type": "integer", "description": "读多少行,缺省读全部"}
                    }),
                    &["path"],
                ),
            ),
            (
                "write_file",
                "创建新文件或整体覆写已有文件。覆写已有文件前系统会自动备份到 undo 目录",
                schema(
                    json!({
                        "path": {"type": "string", "description": "目标文件路径,相对或绝对"},
                        "content": {"type": "string", "description": "完整的新文件内容"}
                    }),
                    &["path", "content"],
                ),
            ),
            (
                "edit",
                "精确替换文件中某段文本(先定位 old_string 再替换为 new_string)。写前自动备份。\
                 若 old_string 出现多次,请带上足够上下文使其唯一",
                schema(
                    json!({
                        "path": {"type": "string", "description": "要修改的文件路径"},
                        "old_string": {"type": "string", "description": "要替换的原文(必须精确匹配)"},
                        "new_string": {"type": "string", "description": "替换成的新文本"}
                    }),
                    &["path", "old_string", "new_string"],
                ),
            ),
            (
                "glob",
                "按文件名/路径模式查找文件,模式支持通配符 * ? 与 ** 递归",
                schema(
                    json!({
                        "pattern": {"type": "string", "description": "glob 模式,如 '**/*.zip' 或 'src/**/*.rs'"},
                        "path": {"type": "string", "description": "搜索起点目录,缺省为当前工作目录"}
                    }),
                    &["pattern"],
                ),
            ),
            (
                "grep_search",
                "在文件中按正则搜索文本内容,返回 路径:行号:内容",
                schema(
                    json!({
                        "pattern": {"type": "string", "description": "正则表达式,如 'TODO|FIXME'"},
                        "path": {"type": "string", "description": "搜索目录或文件,缺省当前工作目录"},
                        "glob": {"type": "string", "description": "只搜索匹配该 glob 的文件,如 '*.rs'"}
                    }),
                    &["pattern"],
                ),
            ),
            (
                "run_shell_command",
                "执行一条终端命令(shell 命令,支持管道/重定向/&& 等)。\
                 高危命令(递归删根/家目录、dd 或 mkfs 写块设备、git push --force 等)会被启发式判定:命中会要求用户确认,\
                 无头模式直接拒绝。判定看的是解析出的目标路径,所以请写明确路径,不要用变量或通配符(会因无法判定而要求确认)",
                schema(
                    json!({
                        "command": {"type": "string", "description": "要执行的完整命令,如 'ls -la'"},
                        "cwd": {"type": "string", "description": "执行目录,缺省为当前工作目录"},
                        "idle_ms": {"type": "integer", "description": "AI 预估的完成时长(毫秒):命令跑超它(含预算 20%、最多 60s 的宽限)仍未结束就会被终止,\
终止时会反馈运行进展与死前输出,由你决定:调大 idle_ms 重试、拆小步骤或换更轻的命令。宁大勿小——超时被杀整轮白跑、重试更慢;\
命令连续无输出超过 90s 会被判卡死(内部规则,无需理会)"}
                    }),
                    &["command"],
                ),
            ),
            (
                "memory_write",
                "写入一条长期记忆(跨会话保留)。适合记录:用户机器环境、常用配置/路径、用户偏好、重要决策。\
                 内容用 markdown,第一句是结论",
                schema(
                    json!({
                        "name": {"type": "string", "description": "记忆名称(短,如 '机器硬件配置')"},
                        "content": {"type": "string", "description": "记忆正文(markdown)"},
                        "description": {"type": "string", "description": "一句话描述(用于索引,可选)"},
                        "mtype": {"type": "string", "description": "类型:user/project/reference/feedback,缺省 project"}
                    }),
                    &["name", "content"],
                ),
            ),
            (
                "memory_read",
                "读取长期记忆。可按名字精确读取,或按关键词搜索。会话开始已自动注入 MEMORY.md 索引摘要",
                schema(
                    json!({
                        "query": {"type": "string", "description": "关键词搜索(可选)"},
                        "name": {"type": "string", "description": "精确读取某条记忆的名字(可选)"}
                    }),
                    &[],
                ),
            ),
            (
                "web_fetch",
                "抓取一个网页(http/https)并转为纯文本。适合查资料、读文档、看新闻。\
                 网络访问受用户网络环境影响,失败时如实说明",
                schema(
                    json!({
                        "url": {"type": "string", "description": "要抓取的完整 URL"},
                        "max_chars": {"type": "integer", "description": "最多返回字符数,缺省 8000"}
                    }),
                    &["url"],
                ),
            ),
        ];
        Self { tools }
    }

    pub fn defs(&self) -> Vec<crate::llm::types::ToolDef> {
        self.tools
            .iter()
            .map(|(name, desc, schema)| {
                crate::llm::types::ToolDef::function(*name, *desc, schema.clone())
            })
            .collect()
    }
}

/// 根据名称分发工具调用
pub async fn execute(
    name: &str,
    args: Value,
    ctx: &ToolContext<'_>,
) -> Result<ToolOutput, ToolError> {
    match name {
        "list_directory" => search::list_directory(ctx, &args).await,
        "read_file" => file::read_file(ctx, &args).await,
        "write_file" => file::write_file(ctx, &args).await,
        "edit" => file::edit(ctx, &args).await,
        "glob" => search::glob(ctx, &args).await,
        "grep_search" => search::grep_search(ctx, &args).await,
        "run_shell_command" => shell::run_shell_command(ctx, &args).await,
        "memory_write" => memory::memory_write(ctx, &args).await,
        "memory_read" => memory::memory_read(ctx, &args).await,
        "web_fetch" => web::web_fetch(ctx, &args).await,
        other => Err(ToolError(format!("未知工具: {other}"))),
    }
}

/// 统一处理权限拒绝
pub fn deny_to_result(d: Decision) -> Result<ToolOutput, ToolError> {
    match d {
        Decision::Allow => Ok(String::new()),
        Decision::Deny(reason) => Err(ToolError(reason)),
    }
}
