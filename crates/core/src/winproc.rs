//! Windows:别让子进程弹黑框。
//!
//! 桌面版是无控制台的 windows 子系统进程,拉起 `cmd` / `powershell` / MCP server
//! 这类控制台程序时,Windows 会给子进程新建一个控制台窗口 —— 用户看到的就是一闪
//! 而过的黑框。加 `CREATE_NO_WINDOW` 就不会建。
//!
//! 只在**本进程没有控制台**时才加:终端版(TUI)本来就有控制台,子进程继承它就
//! 好,贸然给 `CREATE_NO_WINDOW` 会改变子进程看到的环境(某些程序据此判断是否有
//! 终端)。

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(windows)]
fn has_console() -> bool {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetConsoleWindow() -> *mut std::ffi::c_void;
    }
    !unsafe { GetConsoleWindow() }.is_null()
}

/// 给 `std::process::Command` 关掉新控制台窗口(无控制台的进程才需要)。
#[cfg(windows)]
pub fn hide_console_window(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    if !has_console() {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
}

/// 同上,给 `tokio::process::Command` 用(命令行工具、MCP server 走这个)。
#[cfg(windows)]
pub fn hide_console_window_async(cmd: &mut tokio::process::Command) {
    if !has_console() {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
}

#[cfg(not(windows))]
pub fn hide_console_window(_cmd: &mut std::process::Command) {}

#[cfg(not(windows))]
pub fn hide_console_window_async(_cmd: &mut tokio::process::Command) {}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn flags_are_applied_only_without_console() {
        // 测试进程通常没有控制台(cargo test 有),这里只验证不 panic 且幂等
        let mut c = std::process::Command::new("cmd");
        super::hide_console_window(&mut c);
        super::hide_console_window(&mut c);
    }
}
