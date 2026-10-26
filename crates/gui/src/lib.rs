//! znaide 桌面客户端(GUI 骨架)。
//!
//! 形态:本地 HTTP + WebSocket 服务 + 内嵌页面。第 2 步先开浏览器跑通链路;
//! 原生窗口(`wry`)在第 7 步接上,加载的是同一个 URL,前端不返工。
//!
//! 依赖方向:`gui → host → core`(与 `tui` 之间零引用)。这里**不碰引擎**:
//! 启动准备、会话任务、权限/轮数政策全部来自 `znaide-host`。

pub mod bridge;
pub mod server;
pub mod settings;
