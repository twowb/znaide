//! 架构边界:GUI 是独立客户端,只许依赖 core + host,不许碰 TUI 的终端库。
//! 与 `crates/host/tests/boundary.rs` 同一思路(读 manifest,不让依赖悄悄长回来)。

#[test]
fn gui_does_not_depend_on_terminal_ui() {
    let manifest = include_str!("../Cargo.toml");
    for forbidden in ["znaide-tui", "ratatui", "crossterm"] {
        assert!(
            !manifest.contains(forbidden),
            "GUI 不应依赖 {forbidden}:两个客户端之间零引用"
        );
    }
}

#[test]
fn gui_is_built_on_host() {
    let manifest = include_str!("../Cargo.toml");
    assert!(
        manifest.contains("znaide-host"),
        "GUI 的引擎驱动必须走宿主层,不许自己抄一份"
    );
}
