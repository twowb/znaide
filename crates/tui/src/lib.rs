//! znaide 交互界面(TUI):ratatui 渲染,消费 core 会话引擎。

pub mod ansi;
pub mod app;
pub mod md;
pub mod config_ui;
pub mod sessions_ui;

use znaide_core::config::Resolved;
use znaide_core::permissions::Mode;
use std::path::{Path, PathBuf};

pub use app::ExitStats;

/// 启动交互模式。resolved:模型连接配置;mode:权限模式;cwd:agent 工作目录;
/// first_run:首次运行(无配置文件),启动后自动进入配置引导;
/// resume:启动时直接恢复的历史会话文件(--resume <会话ID|片段>);
/// persona:启动时注入的全局人格(空 = 不注入)。
/// 正常退出(/quit、/exit、Ctrl+C)返回本次会话统计供 CLI 打印。
pub async fn run(
    resolved: &Resolved,
    mode: Mode,
    cwd: &Path,
    first_run: bool,
    resume: Option<PathBuf>,
    persona: String,
) -> anyhow::Result<ExitStats> {
    app::run(resolved, mode, cwd, first_run, resume, persona).await
}
