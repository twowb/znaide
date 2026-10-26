//! 原生窗口。
//!
//! Linux:直接用 GTK + WebKitGTK(一个窗口,WebView 是普通控件),不用 wry 的
//! X11 子窗口 —— 子窗口在部分桌面环境下拿不到输入焦点,键盘/点击/滚动会失灵。
//! Windows/macOS:仍用 tao + wry(系统 WebView)。

use std::sync::mpsc::Receiver;

#[cfg(target_os = "linux")]
pub fn run(url: &str, shutdown: Receiver<()>) -> anyhow::Result<()> {
    use gtk::prelude::*;
    use webkit2gtk::WebViewExt;

    gtk::init()?;
    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_title(crate::PRODUCT);
    window.set_default_size(1200, 820);
    window.set_size_request(880, 560);

    let view = webkit2gtk::WebView::new();
    // 右键菜单由页面统一绘制(三端一致);这里只抑制 WebKit 自带菜单
    view.connect_context_menu(|_, _, _, _| true);
    view.load_uri(url);
    window.add(&view);

    window.connect_destroy(|_| gtk::main_quit());
    window.show_all();

    // 页面 /quit 或 Ctrl+C → 结束 GTK 主循环
    let ctx = gtk::glib::MainContext::default();
    std::thread::spawn(move || {
        if shutdown.recv().is_ok() {
            ctx.invoke(gtk::main_quit);
        }
    });

    gtk::main();
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn run(url: &str, shutdown: Receiver<()>) -> anyhow::Result<()> {
    use tao::dpi::LogicalSize;
    use tao::event::{Event, WindowEvent};
    use tao::event_loop::{ControlFlow, EventLoopBuilder};
    use tao::window::WindowBuilder;

    let event_loop = EventLoopBuilder::<()>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    std::thread::spawn(move || {
        if shutdown.recv().is_ok() {
            let _ = proxy.send_event(());
        }
    });

    crate::diag::log("创建原生窗口");
    let window = WindowBuilder::new()
        .with_title(crate::PRODUCT)
        .with_inner_size(LogicalSize::new(1200.0, 820.0))
        .with_min_inner_size(LogicalSize::new(880.0, 560.0))
        .build(&event_loop)?;

    #[cfg(windows)]
    apply_exe_icon(&window);

    // WebView2 默认会在 exe 旁建 <exe>.WebView2;指到 ~/.znaide/webview,
    // 安装目录保持干净,升级替换 exe 也不影响缓存。
    crate::diag::log("原生窗口已创建,准备 WebView2");
    let web_data = znaide_core::config::data_dir().join("webview");
    let _ = std::fs::create_dir_all(&web_data);
    crate::diag::log(format!("WebView2 数据目录:{}", web_data.display()));
    let mut web_context = wry::WebContext::new(Some(web_data));
    let webview = wry::WebViewBuilder::new_with_web_context(&mut web_context)
        .with_devtools(cfg!(debug_assertions))
        .with_url(url)
        .build(&window)?;
    crate::diag::log("WebView2 已就绪");

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            }
            | Event::WindowEvent {
                event: WindowEvent::Destroyed,
                ..
            }
            | Event::UserEvent(()) => *control_flow = ControlFlow::Exit,
            Event::WindowEvent {
                event: WindowEvent::Focused(true),
                ..
            } => {
                let _ = webview.focus();
            }
            _ => {}
        }
    });
}

/// 标题栏/任务栏图标。
///
/// tao 自己注册窗口类时不带图标,Windows 于是显示系统默认的空白图标(资源管理器里
/// 看到的 exe 图标不会自动跑到窗口上)。这里从 exe 自己的资源里取(`build.rs` 用
/// windres 编进去的 id 1),按系统大/小图标尺寸 `LoadImage` 后 `WM_SETICON`。
#[cfg(windows)]
fn apply_exe_icon(window: &tao::window::Window) {
    use std::ffi::c_void;
    use tao::platform::windows::WindowExtWindows;

    #[link(name = "user32")]
    extern "system" {
        fn LoadImageW(
            inst: *mut c_void,
            name: *const u16,
            ty: u32,
            cx: i32,
            cy: i32,
            load: u32,
        ) -> *mut c_void;
        fn SendMessageW(hwnd: *mut c_void, msg: u32, wp: usize, lp: isize) -> isize;
        fn GetSystemMetrics(index: i32) -> i32;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetModuleHandleW(name: *const u16) -> *mut c_void;
    }

    const IMAGE_ICON: u32 = 1;
    const LR_SHARED: u32 = 0x8000;
    const WM_SETICON: u32 = 0x0080;
    const ICON_SMALL: usize = 0;
    const ICON_BIG: usize = 1;
    const SM_CXICON: i32 = 11;
    const SM_CYICON: i32 = 12;
    const SM_CXSMICON: i32 = 49;
    const SM_CYSMICON: i32 = 50;

    let hinst = unsafe { GetModuleHandleW(std::ptr::null()) };
    // tao 的 hwnd() 在不同版本里是 *mut c_void 或 isize,统一走整数中转
    let hwnd = window.hwnd() as isize as *mut c_void;
    let sizes = [
        (
            ICON_SMALL,
            unsafe { GetSystemMetrics(SM_CXSMICON) },
            unsafe { GetSystemMetrics(SM_CYSMICON) },
        ),
        (ICON_BIG, unsafe { GetSystemMetrics(SM_CXICON) }, unsafe {
            GetSystemMetrics(SM_CYICON)
        }),
    ];
    for (which, cx, cy) in sizes {
        // MAKEINTRESOURCE(1):把整数 id 直接当指针传
        let hicon = unsafe { LoadImageW(hinst, 1 as *const u16, IMAGE_ICON, cx, cy, LR_SHARED) };
        if !hicon.is_null() {
            unsafe {
                SendMessageW(hwnd, WM_SETICON, which, hicon as isize);
            }
        }
    }
    crate::diag::log("窗口图标已设置");
}
