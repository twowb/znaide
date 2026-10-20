//! 宿主层的边界守卫:本 crate 不许依赖终端/渲染库。
//!
//! 这条不是形式主义 —— 一旦有人为了"顺手"把 ratatui 或 crossterm 引进来,
//! `gui → host` 就会拖上一个终端栈,GUI 档(Linux glibc 动态 / WebView)直接编不过。
#[test]
fn host_has_no_terminal_dependencies() {
    let manifest = include_str!("../Cargo.toml");
    for banned in ["ratatui", "crossterm"] {
        assert!(
            !manifest.contains(banned),
            "znaide-host 不该依赖 {banned}(宿主层必须与终端无关)"
        );
    }
}
