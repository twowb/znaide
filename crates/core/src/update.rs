//! 自动更新:多源(Gitee → GitHub)探测/下载/安装新版本。
//!
//! - **Gitee 源**(默认优先):国内直连快、无需代理,走 API v5 `releases/latest`
//!   (公开仓库匿名可读),资产下载 URL 与 GitHub 同构
//!   (`/{repo}/releases/download/v{版本}/{资产名}`)。
//! - **GitHub 源**(兜底):版本探测走 `releases/latest` 的 302 重定向
//!   (Location 里带 tag),避开 GitHub API 的匿名限流;资产用直链按平台
//!   命名规范拼(`znaide-<平台>-v<版本>`,与发布脚本的命名强耦合)。
//! - 顺序:Gitee 失败(网络/解析)自动切 GitHub;下载失败同样切下一个源;
//!   全部失败按源给出各自提示(GitHub 提示 HTTPS_PROXY)。
//! - 下载尊重 `HTTPS_PROXY` / `ALL_PROXY` 等代理环境变量。
//! - 自替换:Unix 上运行中的进程可以原子 rename 覆盖自身,下次启动生效;
//!   Windows 运行中的 exe 被系统锁定,改为落一个"退出后自动替换"的
//!   批处理,由 cmd 脱离进程执行(轮询等到 exe 解锁再换)。
//! - 无签名体系:安装前会运行下载产物并核对 `--version` 输出,防下载损坏。

use std::path::{Path, PathBuf};

/// GitHub 发布仓库
pub const REPO: &str = "twowb/znaide";
/// Gitee 发布仓库(与 GitHub 同步的镜像)
pub const GITEE_REPO: &str = "brother-ershui/znaide";

/// 更新源(顺序即回退顺序)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateSource {
    GitHub,
    Gitee,
}

/// 更新源顺序:Gitee 优先(国内直连快),GitHub 兜底
pub const UPDATE_SOURCES: &[UpdateSource] = &[UpdateSource::Gitee, UpdateSource::GitHub];

impl UpdateSource {
    pub fn label(self) -> &'static str {
        match self {
            UpdateSource::GitHub => "GitHub",
            UpdateSource::Gitee => "Gitee",
        }
    }

    fn repo(self) -> &'static str {
        match self {
            UpdateSource::GitHub => REPO,
            UpdateSource::Gitee => GITEE_REPO,
        }
    }

    fn download_host(self) -> &'static str {
        match self {
            UpdateSource::GitHub => "github.com",
            UpdateSource::Gitee => "gitee.com",
        }
    }

    /// 探测该源的最新发布版本(不带 v 前缀,如 "1.1.0")
    pub async fn check_latest(self, client: &reqwest::Client) -> anyhow::Result<String> {
        match self {
            UpdateSource::GitHub => {
                let url = format!("https://github.com/{}/releases/latest", self.repo());
                let resp = client.head(&url).send().await?;
                if !resp.status().is_success() {
                    anyhow::bail!("HTTP {}", resp.status());
                }
                let path = resp.url().path().to_string();
                // 形如 /twowb/znaide/releases/tag/v1.2.3(旧格式 /releases/v1.2.3)
                let tag = path
                    .rsplit('/')
                    .find(|seg| !seg.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("无法解析重定向地址: {path}"))?;
                parse_version_from_tag(tag)
            }
            UpdateSource::Gitee => {
                // API v5 releases/latest:公开仓库匿名可读,一次拿最新 tag
                let url = format!(
                    "https://gitee.com/api/v5/repos/{}/releases/latest",
                    self.repo()
                );
                let resp = client.get(&url).send().await?;
                if !resp.status().is_success() {
                    anyhow::bail!("HTTP {} ({url})", resp.status());
                }
                let text = resp.text().await?;
                let v: serde_json::Value = serde_json::from_str(&text)
                    .map_err(|e| anyhow::anyhow!("Gitee 返回解析失败: {e}"))?;
                let tag = v
                    .get("tag_name")
                    .and_then(|t| t.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Gitee 响应缺少 tag_name"))?;
                parse_version_from_tag(tag)
            }
        }
    }

    /// 该源指定版本的资产直链(两端 URL 形态一致)
    fn download_url(self, version: &str) -> anyhow::Result<String> {
        let fname = release_asset_name(version)
            .ok_or_else(|| anyhow::anyhow!("当前平台没有发布产物,无法自动更新"))?;
        Ok(format!(
            "https://{}/{}/releases/download/v{version}/{fname}",
            self.download_host(),
            self.repo()
        ))
    }
}

/// 从 tag 段(v1.2.3 / 1.2.3)解析出版本号
fn parse_version_from_tag(tag: &str) -> anyhow::Result<String> {
    let ver = tag.trim_start_matches('v');
    if ver.is_empty() || !ver.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        anyhow::bail!("无法解析版本号: {tag}");
    }
    Ok(ver.to_string())
}

/// 当前程序版本
pub fn current_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// 发布资产全名:znaide-<平台>-v<版本><扩展名>。
/// 命名规范(与 Makefile 产物一致):平台段 linux-x64 / linux-arm64 /
/// windows-x64 / macos-x64 / macos-arm64 / android-arm64,Windows 的
/// .exe 在版本号**之后**(znaide-windows-x64-v1.0.1.exe)。
pub fn release_asset_name(version: &str) -> Option<String> {
    // 用 cfg! 而不是 #[cfg] 块:后者在当前 target 上会让前面的块提前 return,
    // 末尾的 None 被判"不可达"报警告(但它在未覆盖的架构上其实是要走的分支)。
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some(format!("znaide-linux-x64-v{version}"))
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some(format!("znaide-linux-arm64-v{version}"))
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some(format!("znaide-windows-x64-v{version}.exe"))
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some(format!("znaide-macos-x64-v{version}"))
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some(format!("znaide-macos-arm64-v{version}"))
    } else if cfg!(all(target_os = "android", target_arch = "aarch64")) {
        Some(format!("znaide-android-arm64-v{version}"))
    } else {
        None
    }
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

/// 依次探测各源,返回**第一个连通源**的最新版本。
/// 全部失败才 Err(TUI 启动静默检查与展示层用)。
pub async fn probe_latest(
    client: &reqwest::Client,
) -> anyhow::Result<(UpdateSource, String)> {
    let mut errs: Vec<String> = Vec::new();
    for &src in UPDATE_SOURCES {
        match src.check_latest(client).await {
            Ok(v) => return Ok((src, v)),
            Err(e) => errs.push(format!("{}: {e:#}", src.label())),
        }
    }
    anyhow::bail!("所有更新源均不可达({})", errs.join("; "))
}

/// 从指定源下载指定版本的二进制到可执行文件同目录的临时文件,返回其路径
pub async fn download_from(
    client: &reqwest::Client,
    src: UpdateSource,
    version: &str,
) -> anyhow::Result<PathBuf> {
    let url = src.download_url(version)?;
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
    /// 更新完成;source = 下载来源;deferred=true 表示替换要等进程退出后由批处理完成
    Updated {
        version: String,
        source: &'static str,
        deferred: bool,
    },
    /// 检查失败(网络/代理/解析)
    CheckFailed(String),
    /// 下载失败
    DownloadFailed(String),
    /// 下载产物自检不过
    VerifyFailed(String),
}

impl UpdateResult {
    /// 给用户看的一句话(CLI 打印 / TUI 提示共用一份,别再各写一遍 match)。
    /// `current` = 当前版本(已是最新时回显)。
    pub fn describe(&self, current: &str) -> String {
        match self {
            UpdateResult::UpToDate => format!("已是最新版本 v{current}。"),
            UpdateResult::Updated {
                version,
                source,
                deferred: false,
            } => format!("✔ 已更新到 v{version}(来源 {source}):下次启动生效(本次继续用旧版本)。"),
            UpdateResult::Updated {
                version,
                source,
                deferred: true,
            } => format!(
                "✔ 新版本 v{version}(来源 {source})已就位:退出程序后自动完成替换,下次启动生效。"
            ),
            UpdateResult::CheckFailed(e) => format!("⚠ 检查更新失败: {e}"),
            UpdateResult::DownloadFailed(e) => format!("⚠ 更新失败: {e}"),
            UpdateResult::VerifyFailed(e) => format!("⚠ {e}"),
        }
    }
}

/// 完整执行一次更新:按源顺序[Gitee → GitHub]探测 → 比较 → 下载 → 自检 → 安装。
/// - 首个能连通(探测成功)的源决定"最新版本":已是最新即结束;
///   有新版就从该源下载,下载失败自动尝试其它源(同版本资产)。
/// - 全部源探测失败 → CheckFailed,按源列出原因(GitHub 失败附 HTTPS_PROXY 提示)。
/// 不发网络请求的前提错误(代理构造失败)也并入 CheckFailed。
pub async fn perform_update() -> UpdateResult {
    let cur = current_version();
    let client = match http_client() {
        Ok(c) => c,
        Err(e) => return UpdateResult::CheckFailed(format!("客户端构造失败: {e:#}")),
    };

    // 各源探测失败记录(全败时汇总给用户)
    let mut check_errs: Vec<String> = Vec::new();
    for &src in UPDATE_SOURCES {
        let latest = match src.check_latest(&client).await {
            Ok(v) => v,
            Err(e) => {
                check_errs.push(format!("{}: {e:#}", src.label()));
                continue;
            }
        };
        // 首个连通源:以它判定是否已最新
        if !version_gt(&latest, &cur) {
            return UpdateResult::UpToDate;
        }
        // 有新版:优先本源下载;失败再按源表顺序试其它源(同版本资产名一致)
        let mut dl_errs: Vec<String> = Vec::new();
        let mut dl_order = Vec::with_capacity(UPDATE_SOURCES.len());
        dl_order.push(src);
        dl_order.extend(UPDATE_SOURCES.iter().copied().filter(|&s| s != src));
        for s2 in dl_order {
            match download_from(&client, s2, &latest).await {
                Ok(tmp) => {
                    if let Err(e) = verify_download(&tmp, &latest) {
                        let _ = std::fs::remove_file(&tmp);
                        return UpdateResult::VerifyFailed(format!("{e:#}"));
                    }
                    let source_label = s2.label();
                    return match install_update(&tmp) {
                        Ok(InstallOutcome::Replaced) => UpdateResult::Updated {
                            version: latest,
                            source: source_label,
                            deferred: false,
                        },
                        Ok(InstallOutcome::Deferred) => UpdateResult::Updated {
                            version: latest,
                            source: source_label,
                            deferred: true,
                        },
                        Err(e) => UpdateResult::DownloadFailed(format!("安装失败: {e:#}")),
                    };
                }
                Err(e) => dl_errs.push(format!("{}: {e:#}", s2.label())),
            }
        }
        return UpdateResult::DownloadFailed(format!(
            "探测到新版 v{latest} 但下载失败:\n{}",
            dl_errs.join("\n")
        ));
    }

    // 全部源探测失败
    let github_hint = check_errs
        .iter()
        .any(|e| e.starts_with("GitHub:"))
        .then(|| "\n提示:更新源含 GitHub,网络不通时可设置 HTTPS_PROXY 环境变量")
        .unwrap_or("");
    UpdateResult::CheckFailed(format!("检查更新失败:\n{}{github_hint}", check_errs.join("\n")))
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

    #[test]
    fn update_sources_gitee_first_then_github() {
        // 顺序即回退顺序:Gitee 优先(国内直连快),GitHub 兜底
        assert_eq!(UPDATE_SOURCES.len(), 2);
        assert_eq!(UPDATE_SOURCES[0], UpdateSource::Gitee);
        assert_eq!(UPDATE_SOURCES[1], UpdateSource::GitHub);
        assert_eq!(UpdateSource::GitHub.label(), "GitHub");
        assert_eq!(UpdateSource::Gitee.label(), "Gitee");
    }

    #[test]
    fn parse_version_handles_v_prefix_and_garbage() {
        assert_eq!(parse_version_from_tag("v1.2.3").unwrap(), "1.2.3");
        assert_eq!(parse_version_from_tag("1.2.3").unwrap(), "1.2.3");
        assert!(parse_version_from_tag("release").is_err());
        assert!(parse_version_from_tag("v").is_err());
        assert!(parse_version_from_tag("").is_err());
    }

    #[test]
    fn download_url_matches_both_hosts() {
        // GitHub 与 Gitee 资产 URL 同构(实测 Gitee browser_download_url 即此形态),
        // 平台段与 .exe 位置决定成败(线上踩过 404 的坑)
        let gh = UpdateSource::GitHub.download_url("1.0.2").unwrap();
        assert!(gh.starts_with("https://github.com/twowb/znaide/releases/download/v1.0.2/"));
        let gi = UpdateSource::Gitee.download_url("1.0.2").unwrap();
        assert!(gi.starts_with("https://gitee.com/brother-ershui/znaide/releases/download/v1.0.2/"));
        // 资产名片段一致(两端发布同名资产)
        let fname = release_asset_name("1.0.2").unwrap();
        assert!(gh.ends_with(&format!("/{fname}")));
        assert!(gi.ends_with(&format!("/{fname}")));
    }
}
