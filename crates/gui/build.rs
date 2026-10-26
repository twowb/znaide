//! Windows 单文件:把 WebView2Loader 静态链进 exe,运行时不再需要 DLL。
//! MSVC 目标本身就会走 WebView2LoaderStatic;这里补 GNU 目标(从 cargo 缓存找
//! WebView2LoaderStatic.lib,--whole-archive 强制拉入,避免被 import lib 抢先)。

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=packaging/znaide-gui.ico");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if os != "windows" {
        return;
    }
    embed_icon();
}

fn embed_icon() {
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let tool = match arch.as_str() {
        "x86_64" => "x86_64-w64-mingw32-windres",
        "x86" => "i686-w64-mingw32-windres",
        "aarch64" => "aarch64-w64-mingw32-windres",
        _ => return,
    };
    let pkg =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default()).join("packaging");
    let ico = pkg.join("znaide-gui.ico");
    if !ico.is_file() {
        return;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap_or_default());
    let rc = out.join("znaide-gui.rc");
    let obj = out.join("znaide-gui-res.o");
    let ver = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let nums: Vec<u32> = ver.split('.').map(|x| x.parse().unwrap_or(0)).collect();
    let v = format!(
        "{},{},{},0",
        nums.first().unwrap_or(&0),
        nums.get(1).unwrap_or(&0),
        nums.get(2).unwrap_or(&0)
    );
    let content = format!(
        "#pragma code_page(65001)\n1 ICON \"{}\"\n1 VERSIONINFO\nFILEVERSION {v}\nPRODUCTVERSION {v}\nBEGIN\nBLOCK \"StringFileInfo\"\nBEGIN\nBLOCK \"040904B0\"\nBEGIN\nVALUE \"ProductName\", \"修凡AI全能助手\\0\"\nVALUE \"FileDescription\", \"修凡AI全能助手（基于 znaide 引擎）\\0\"\nVALUE \"FileVersion\", \"{ver}\\0\"\nVALUE \"ProductVersion\", \"{ver}\\0\"\nEND\nEND\nBLOCK \"VarFileInfo\"\nBEGIN\nVALUE \"Translation\", 0x409, 1200\nEND\nEND\n",
        ico.display()
    );
    let _ = std::fs::write(&rc, content);
    if let Ok(s) = Command::new(tool)
        .arg("-i")
        .arg(&rc)
        .arg("-o")
        .arg(&obj)
        .status()
    {
        if s.success() {
            println!("cargo:rustc-link-arg={}", obj.display());
        }
    }
}
