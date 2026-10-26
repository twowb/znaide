//! 启动期诊断:别让"没有控制台"把失败吞掉。
//!
//! release 是 `windows_subsystem = "windows"`(无控制台)且 `panic = "abort"`,于是
//! ① 双击时 `println!` 写失败会 panic 成静默闪退;② `anyhow` 错误只进 stderr,谁也
//! 看不见。这里补三件事:附加父控制台、把失败写进 `~/.znaide/gui.log`、无控制台时
//! 弹 MessageBox。所有输出都忽略写失败,诊断代码自己不能再成为崩溃源。

use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Instant;

const MAX_LOG: u64 = 256 * 1024;

static START: OnceLock<Instant> = OnceLock::new();

pub fn log_path() -> PathBuf {
    znaide_core::config::data_dir().join("gui.log")
}

/// 进程一进来就调:先接控制台,再装 panic 钩子,最后落一行启动横幅。
pub fn init() {
    START.get_or_init(Instant::now);
    attach_parent_console();
    install_panic_hook();
    let path = log_path();
    if std::fs::metadata(&path)
        .map(|m| m.len() > MAX_LOG)
        .unwrap_or(false)
    {
        let _ = std::fs::remove_file(&path);
    }
    log(format!(
        "== 修凡AI全能助手 {} 启动 (pid {}) ==",
        crate::GUI_VERSION,
        std::process::id()
    ));
}

/// 写日志(失败就放弃)。每行带相对启动的毫秒数,便于看卡在哪一步。
pub fn log(msg: impl AsRef<str>) {
    let start = START.get_or_init(Instant::now);
    let ms = start.elapsed().as_millis();
    if let Some(dir) = log_path().parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
    {
        let _ = writeln!(f, "[+{ms:>6}ms] {}", msg.as_ref());
    }
}

/// 正常输出。控制台不存在 / 句柄被关掉时静默跳过,绝不 panic。
pub fn say(msg: impl AsRef<str>) {
    let _ = writeln!(std::io::stdout(), "{}", msg.as_ref());
}

/// 告警输出。同上,写失败不影响运行。
pub fn warn(msg: impl AsRef<str>) {
    let _ = writeln!(std::io::stderr(), "{}", msg.as_ref());
}

/// 致命错误:进日志 + 控制台 + (无控制台时)弹窗,然后以 1 退出。
pub fn fatal(msg: &str) -> ! {
    log(format!("致命错误:{msg}"));
    warn(format!("错误:{msg}"));
    message_box(
        "修凡AI全能助手启动失败",
        &format!("{msg}\n\n详细日志:\n{}", log_path().display()),
    );
    std::process::exit(1)
}

fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let text = info.to_string();
        let loc = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_default();
        log(format!("panic:{text} ({loc})"));
        warn(format!("内部错误:{text}"));
        message_box(
            "修凡AI全能助手内部错误",
            &format!("{text}\n\n{loc}\n\n详细日志:\n{}", log_path().display()),
        );
        default(info);
    }));
}

#[cfg(windows)]
fn attach_parent_console() {
    #[link(name = "kernel32")]
    extern "system" {
        fn AttachConsole(pid: u32) -> i32;
    }
    const ATTACH_PARENT_PROCESS: u32 = 0xFFFF_FFFF;
    // 从 cmd/PowerShell 启动时把父控制台接过来,`--version`、地址、错误才看得见。
    // 双击(没有父控制台)会失败,失败正是我们想要的:此时走弹窗。
    let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

#[cfg(not(windows))]
fn attach_parent_console() {}

#[cfg(windows)]
fn message_box(caption: &str, text: &str) {
    #[link(name = "user32")]
    extern "system" {
        fn MessageBoxW(
            hwnd: *mut std::ffi::c_void,
            text: *const u16,
            caption: *const u16,
            utype: u32,
        ) -> i32;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetConsoleWindow() -> *mut std::ffi::c_void;
    }
    const MB_ICONERROR: u32 = 0x10;
    const MB_SETFOREGROUND: u32 = 0x1_0000;
    // 已经有控制台就别再弹窗(命令行用户看终端就够了)
    if !unsafe { GetConsoleWindow() }.is_null() {
        warn(text);
        return;
    }
    let wide = |s: &str| {
        let mut v: Vec<u16> = s.encode_utf16().collect();
        v.push(0);
        v
    };
    let (text, caption) = (wide(text), wide(caption));
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            caption.as_ptr(),
            MB_ICONERROR | MB_SETFOREGROUND,
        );
    }
}

#[cfg(not(windows))]
fn message_box(_caption: &str, text: &str) {
    warn(text);
}
