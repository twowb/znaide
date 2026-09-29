//! 网络出口:**全项目唯一**的 HTTP 客户端工厂。
//!
//! 以前三处各建各的客户端(LLM / 网页抓取 / 更新下载),更新那处还自己手写了一段
//! 环境变量探测,于是代理配置永远对不齐 —— 于是有了这个模块:一个入口
//! [`client_builder`],三个调用点,代理取同一个 [`EffectiveProxy`] 快照。

use crate::config::EffectiveProxy;

/// 标准代理环境变量的读取顺序(与 reqwest 的口径一致:https 目标看 HTTPS_PROXY,
/// 任意目标看 ALL_PROXY,http 目标看 HTTP_PROXY;大写优先)
const ENV_PROXY_VARS: &[&str] = &[
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
    "HTTP_PROXY",
    "http_proxy",
];

/// 环境里第一个非空的标准代理变量
pub fn env_proxy() -> Option<(&'static str, String)> {
    for name in ENV_PROXY_VARS {
        if let Ok(v) = std::env::var(name) {
            let v = v.trim().to_string();
            if !v.is_empty() {
                return Some((name, v));
            }
        }
    }
    None
}

/// 代理地址校验:只认 http/https 且必须有主机。
/// (socks5 需要给 reqwest 开 `socks` feature,暂时不支持;要加再说。)
pub fn valid_proxy_url(url: &str) -> bool {
    let u = url.trim();
    let rest = match u.strip_prefix("http://").or_else(|| u.strip_prefix("https://")) {
        Some(r) => r,
        None => return false,
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or(authority);
    !host.is_empty()
}

/// 代理地址脱敏:去掉 `user:pass@` 与路径/查询(都可能带密钥),只留 `scheme://host[:port]`。
/// 任何要展示、要写日志、要塞进错误的地方必须先过这一道。
pub fn mask_proxy_url(url: &str) -> String {
    let u = url.trim();
    let (scheme, rest) = match u.split_once("://") {
        Some((s, r)) => (s, r),
        None => ("", u),
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or(authority);
    if scheme.is_empty() {
        host.to_string()
    } else {
        format!("{scheme}://{host}")
    }
}

/// 当前生效代理的人读描述,给错误文案用 ——
/// "代理没起"和"端点不通"是两回事,不给线索就只能瞎猜。
pub fn describe_proxy(p: &EffectiveProxy) -> String {
    match p {
        EffectiveProxy::Manual(u) => format!("当前代理:{}", mask_proxy_url(u)),
        EffectiveProxy::Direct => "当前:强制直连(没用代理)".to_string(),
        EffectiveProxy::Auto => match env_proxy() {
            Some((name, v)) => format!("当前:跟随环境变量 {name}={}", mask_proxy_url(&v)),
            None => "当前:直连(没检测到代理环境变量)".to_string(),
        },
    }
}

/// 建 HTTP 客户端:全项目唯一入口。超时/UA 由调用方按场景自己加
/// (LLM 600s、更新 240s、抓网页 30s 都是有意为之,这里不统一)。
///
/// - [`EffectiveProxy::Auto`]:什么都不设,交给 reqwest 读标准环境变量(与旧行为一致)
/// - [`EffectiveProxy::Direct`]:`.no_proxy()`,连环境变量一起无视(排查/内网)
/// - [`EffectiveProxy::Manual`]:走指定代理,但**仍尊重 `NO_PROXY` 豁免**
///   ("国内服务商直连 + 国外走代理"是常见用法,不给豁免就只能全局绕一圈)
pub fn client_builder(proxy: &EffectiveProxy) -> anyhow::Result<reqwest::ClientBuilder> {
    let b = reqwest::Client::builder();
    let b = match proxy {
        EffectiveProxy::Auto => b,
        EffectiveProxy::Direct => b.no_proxy(),
        EffectiveProxy::Manual(url) => {
            let p = reqwest::Proxy::all(url).map_err(|e| {
                anyhow::anyhow!("代理地址无法解析({}):{e}", mask_proxy_url(url))
            })?;
            b.proxy(p.no_proxy(reqwest::NoProxy::from_env()))
        }
    };
    Ok(b)
}
