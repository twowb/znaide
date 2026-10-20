//! 更新检查与安装:语义(探测顺序、版本比较、给用户的那句话)放这里,
//! 前端只负责"什么时候起任务、把结果放哪显示"。
//!
//! 更新下载也是网络出口之一,必须走统一代理(`EffectiveProxy`)—— 别绕过。

use znaide_core::config::EffectiveProxy;

/// 静默探测是否有新版本。有则返回要提示给用户的一句话,没有/连不上返回 `None`
/// (网络失败不打扰,用户随时可以 `/update` 手动检查)。
pub async fn check_notice(proxy: &EffectiveProxy) -> Option<String> {
    let client = znaide_core::update::http_client(proxy).ok()?;
    let (src, latest) = znaide_core::update::probe_latest(&client).await.ok()?;
    let cur = znaide_core::update::current_version();
    if !znaide_core::update::version_gt(&latest, &cur) {
        return None;
    }
    Some(format!(
        "发现新版本 v{latest}(来源 {})\n当前 v{cur}:输入 /update 立即更新,或忽略继续使用。",
        src.label()
    ))
}

/// 检查并安装最新版,返回给用户的一句话(下载可能要一阵,调用方放后台任务里跑)。
pub async fn install(proxy: &EffectiveProxy) -> String {
    let cur = znaide_core::update::current_version();
    znaide_core::update::perform_update(proxy).await.describe(&cur)
}
