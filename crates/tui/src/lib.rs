//! znaide 交互界面(TUI):ratatui 渲染,消费 core 会话引擎。

pub mod ansi;
pub mod app;
pub mod md;
pub mod config_ui;
pub mod sessions_ui;
#[cfg(test)]
pub mod test_util;

use znaide_core::permissions::Mode;

pub use app::ExitStats;

/// 入口:宿主层已把配置/首启/恢复目标/重试/轮数都算好(`host::prepare`),
/// TUI 只决定"怎么显示、按键怎么映射"。`mode` 是本次权限档(命令行 `--permission`)。
/// 正常退出(/quit、/exit、Ctrl+C)返回本次会话统计供 CLI 打印。
pub async fn run(prepared: &znaide_host::Prepared, mode: Mode) -> anyhow::Result<ExitStats> {
    app::run(prepared, mode).await
}
