pub mod config;
pub mod llm;
pub mod mcp;
pub mod permissions;
pub mod persona;
pub mod refs;
pub mod session;
pub mod skills;
pub mod tools;
pub mod undo;
pub mod update;
pub mod util;

/// 测试共享设施。依赖进程级环境变量(ZNAIDE_DATA_DIR)的测试必须
/// 先拿这把锁,否则并行测试会互相覆盖 env / 竞争共享 manifest 文件。
#[cfg(test)]
pub mod test_util {
    /// 串行化依赖共享数据目录的测试(guard 存活到测试结束即互斥)
    pub static DATA_DIR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}
