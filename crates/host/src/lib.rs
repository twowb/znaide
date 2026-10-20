//! znaide 的**宿主层**:驱动引擎的那一层,与界面无关。
//!
//! 边界(靠 `tests/boundary.rs` 守着):本 crate **不依赖任何终端/渲染库**
//! (`ratatui` / `crossterm`),于是 TUI 与将来的 GUI 能共用同一份宿主逻辑 ——
//! 动作面、权限与轮数询问的**回答政策**、投递箱/取消槽、slash 语义都只有一份。
//!
//! 依赖方向:`tui → host → core`、`gui → host → core`(两个客户端之间零引用)。

pub mod agent;
pub mod policy;
pub mod prepare;

pub use agent::{spawn, AgentCmd, AgentConfig, AgentHandle, SharedCancel};
pub use prepare::{prepare, Note, NoteLevel, Prepared, StartupArgs};
