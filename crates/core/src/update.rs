//! 自动更新:从 GitHub Releases 探测/下载/安装新版本。
//!
//! - 版本探测走 `releases/latest` 的 302 重定向(Location 里带 tag),避开
//!   GitHub API 的匿名限流;资产用直链按平台命名规范拼
//!   (`znaide-<平台>-v<版本>`),与发布脚本的命名强耦合。
//! - 下载尊重 `HTTPS_PROXY` / `ALL_PROXY` 等代理环境变量(GitHub 直连
//!   常不通,由用户侧代理转发)。
//! - 自替换:Unix 上运行中的进程可以原子 rename 覆盖自身,下次启动生效;
//!   Windows 运行中的 exe 被系统锁定,改为落一个"退出后自动替换"的
//!   批处理,由 cmd 脱离进程执行(轮询等到 exe 解锁再换)。
//! - 无签名体系:安装前会运行下载产物并核对 `--version` 输出,防下载损坏。

use std::path::{Path, PathBuf};

/// 发布仓库
pub const REPO: &str = "twowb/znaide";

/// 当前程序版本
pub fn current_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// 发布资产全名:znaide-<平台>-v<版本><扩展名>。
/// 命名规范(与 Makefile 产物一致):平台段 linux-x64 / linux-arm64 /
/// windows-x64 / macos-x64 / macos-arm64 / android-arm64,Windows 的
/// .exe 在版本号**之后**(znaide-windows-x64-v1.0.1.exe)。
pub fn release_asset_name(version: &str) -> Option<String> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        return Some(format!("znaide-linux-x64-v{version}"));
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        return Some(format!("znaide-linux-arm64-v{version}"));
    }
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        return Some(format!("znaide-windows-x64-v{version}.exe"));
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        return Some(format!("znaide-macos-x64-v{version}"));
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        return Some(format!("znaide-macos-arm64-v{version}"));
    }
    #[cfg(all(target_os = "android", target_arch = "aarch64"))]
    {
        return Some(format!("znaide-android-arm64-v{version}"));
    }
    None
}

/// 语义化版本 vX.Y.Z 比较:a > b?
pub fn version_gt(a: &str, b: &str) -> bool {
    fn nums(v: &str) -> Vec<u64> {
        v.trim_start_matches('v')
            .split(['.', '-', '+'])
            .filter_map(|s| s.parse::<u64>().ok())
            .collect()
    }
    let (na, nb) = (nums(a), nums(b));
    for i in 0..na.len().max(nb.len()) {
        let x = na.get(i).copied().unwrap_or(0);
        let y = nb.get(i).copied().unwrap_or(0);
        if x != y {
            return x > y;
        }
    }
    false
}

/// 组装发布下载用 HTTP 客户端(尊重代理环境变量)
pub fn http_client() -> anyhow::Result<reqwest::Client> {
    let mut b = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(240))
        .user_agent(concat!("znaide-updater/", env!("CARGO_PKG_VERSION")));
    for var in [
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "HTTP_PROXY",
        "http_proxy",
    ] {
        if let Ok(v) = std::env::var(var) {
            let v = v.trim().to_string();
            if !v.is_empty() {
                if let Ok(p) = reqwest::Proxy::all(&v) {
                    b = b.proxy(p);
                }
                break;
            }
        }
    }
    Ok(b.build()?)
}

/// 探测最新发布版本(不带 v 前缀的数字段,如 "1.1.0")。
/// 网络失败/解析不出都返回 Err,调用方决定是否提示。
pub async fn check_latest(client: &reqwest::Client) -> anyhow::Result<String> {
    let url = format!("https://github.com/{REPO}/releases/latest");
    let resp = client.head(&url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("探测最新版本失败: HTTP {}", resp.status());
    }
    let path = resp.url().path().to_string();
    // 形如 /twowb/znaide/releases/tag/v1.2.3(旧格式 /releases/v1.2.3)
    let tag = path
        .rsplit('/')
        .find(|seg| !seg.is_empty())
        .ok_or_else(|| anyhow::anyhow!("无法解析重定向地址: {path}"))?;
    let ver = tag.trim_start_matches('v');
    if ver.is_empty() || !ver.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        anyhow::bail!("无法解析版本号: {tag}");
    }
    Ok(ver.to_string())
}

/// 下载指定版本的二进制到可执行文件同目录的临时文件,返回其路径
pub async fn download_update(
    client: &reqwest::Client,
    version: &str,
) -> anyhow::Result<PathBuf> {
    let fname = release_asset_name(version)
        .ok_or_else(|| anyhow::anyhow!("当前平台没有发布产物,无法自动更新"))?;
    let url = format!(
        "https://github.com/{REPO}/releases/download/v{version}/{fname}"
    );
    let resp = client.get(&url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("下载失败: HTTP {} ({url})", resp.status());
    }
    let bytes = resp.bytes().await?;
    if bytes.len() < 100_000 {
        anyhow::bail!("下载产物过小({} 字节),疑似错误响应", bytes.len());
    }
    let exe = std::env::current_exe()?;
    let dir = exe.parent().ok_or_else(|| anyhow::anyhow!("无法定位程序目录"))?;
    let tmp = dir.join(format!(".znaide-update-{version}.tmp{}", exe_extra()));
    std::fs::write(&tmp, &bytes)?;
    make_executable(&tmp)?;
    Ok(tmp)
}

/// Windows 上临时文件也要 .exe 后缀,自检时才能被系统识别为可执行
fn exe_extra() -> &'static str {
    #[cfg(windows)]
    {
        ".exe"
    }
    #[cfg(not(windows))]
    {
        ""
    }
}

#[cfg(unix)]
fn make_executable(p: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perm = std::fs::metadata(p)?.permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(p, perm)?;
    Ok(())
}

#[cfg(windows)]
fn make_executable(_p: &Path) -> anyhow::Result<()> {
    Ok(())
}

/// 安装结果:Unix 已当场替换 / Windows 需等进程退出后由批处理完成
pub enum InstallOutcome {
    /// 已原子替换,下次启动即新版本
    Replaced,
    /// 替换批处理已交给系统,进程退出后自动完成
    Deferred,
}

/// 把已下载的临时文件安装为正式可执行文件
pub fn install_update(downloaded: &Path) -> anyhow::Result<InstallOutcome> {
    let exe = std::env::current_exe()?;
    #[cfg(unix)]
    {
        std::fs::rename(downloaded, &exe)?;
        make_executable(&exe)?;
        Ok(InstallOutcome::Replaced)
    }
    #[cfg(windows)]
    {
        let dir = exe.parent().ok_or_else(|| anyhow::anyhow!("无法定位程序目录"))?;
        let exe_name = exe
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow::anyhow!("程序文件名异常"))?;
        let tmp_name = downloaded
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow::anyhow!("临时文件名字异常"))?;
        let bat = dir.join("znaide-update.bat");
        // 轮询:等本进程退出(exe 解锁)后 rename 成功,再换新文件
        let script = format!(
            "@echo off\r\n\
             :again\r\n\
             timeout /t 1 /nobreak >nul\r\n\
             if exist \"%~dp0{exe_name}.old\" del /f /q \"%~dp0{exe_name}.old\"\r\n\
             ren \"%~dp0{exe_name}\" \"{exe_name}.old\" >nul 2>&1\r\n\
             if errorlevel 1 goto again\r\n\
             move /y \"%~dp0{tmp_name}\" \"%~dp0{exe_name}\" >nul 2>&1\r\n\
             del /f /q \"%~dp0{exe_name}.old\" >nul 2>&1\r\n"
        );
        std::fs::write(&bat, script)?;
        // 脱离父进程执行(start /b),父进程(本程序)退出后批处理继续等锁释放
        let _ = std::process::Command::new("cmd")
            .args(["/c", "start", "", "/b", bat.to_str().unwrap_or_default()])
            .spawn()?;
        Ok(InstallOutcome::Deferred)
    }
}

/// 一次更新尝试的结果(CLI 与 TUI 展示层共用,文本已在各变体里)
pub enum UpdateResult {
    /// 已是最新
    UpToDate,
    /// 更新完成;deferred=true 表示替换要等进程退出后由批处理完成
    Updated { version: String, deferred: bool },
    /// 检查失败(网络/代理/解析)
    CheckFailed(String),
    /// 下载失败
    DownloadFailed(String),
    /// 下载产物自检不过
    VerifyFailed(String),
}

/// 完整执行一次更新:探测 → 比较 → 下载 → 自检 → 安装。
/// 不发网络请求的前提错误(代理构造失败)也并入 CheckFailed。
pub async fn perform_update() -> UpdateResult {
    let cur = current_version();
    let client = match http_client() {
        Ok(c) => c,
        Err(e) => return UpdateResult::CheckFailed(format!("客户端构造失败: {e:#}")),
    };
    let latest = match check_latest(&client).await {
        Ok(v) => v,
        Err(e) => {
            return UpdateResult::CheckFailed(format!(
                "{e:#}\n提示:更新走 GitHub,网络不通时请设置 HTTPS_PROXY 环境变量"
            ))
        }
    };
    if !version_gt(&latest, &cur) {
        return UpdateResult::UpToDate;
    }
    let tmp = match download_update(&client, &latest).await {
        Ok(p) => p,
        Err(e) => return UpdateResult::DownloadFailed(format!("{e:#}")),
    };
    if let Err(e) = verify_download(&tmp, &latest) {
        let _ = std::fs::remove_file(&tmp);
        return UpdateResult::VerifyFailed(format!("{e:#}"));
    }
    match install_update(&tmp) {
        Ok(InstallOutcome::Replaced) => UpdateResult::Updated { version: latest, deferred: false },
        Ok(InstallOutcome::Deferred) => UpdateResult::Updated { version: latest, deferred: true },
        Err(e) => UpdateResult::DownloadFailed(format!("安装失败: {e:#}")),
    }
}

/// 自检下载产物:运行 `--version`,输出应包含目标版本
pub fn verify_download(exe: &Path, expect_version: &str) -> anyhow::Result<()> {
    let out = std::process::Command::new(exe).arg("--version").output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    if !text.contains(expect_version) {
        anyhow::bail!(
            "下载产物自检失败(期望 v{expect_version},输出: {})",
            text.trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare() {
        assert!(version_gt("1.1.0", "1.0.0"));
        assert!(version_gt("1.0.1", "1.0.0"));
        assert!(version_gt("v1.2.0", "1.1.9"));
        assert!(!version_gt("1.0.0", "1.0.0"));
        assert!(!version_gt("1.0.0", "1.1.0"));
        assert!(!version_gt("0.9.9", "1.0.0"));
    }

    #[test]
    fn release_asset_name_matches_naming() {
        // 资产名必须能精确拼出发布文件:znaide-<平台>-v<版本><.exe>。
        // 拼错(漏版本段/把 .exe 放平台段)会导致下载 404——线上踩过的坑
        let name = release_asset_name("1.0.1").expect("当前平台应支持自动更新");
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        assert_eq!(name, "znaide-linux-x64-v1.0.1");
        #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
        assert_eq!(name, "znaide-linux-arm64-v1.0.1");
        #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
        assert_eq!(name, "znaide-windows-x64-v1.0.1.exe");
        #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
        assert_eq!(name, "znaide-macos-x64-v1.0.1");
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        assert_eq!(name, "znaide-macos-arm64-v1.0.1");
        #[cfg(all(target_os = "android", target_arch = "aarch64"))]
        assert_eq!(name, "znaide-android-arm64-v1.0.1");
        // .exe 必须在版本号之后,不在平台段里
        assert!(name.ends_with(".exe") == cfg!(target_os = "windows"));
        assert!(!name.contains(".exe-v"), "扩展名位置错误: {name}");
    }
}
