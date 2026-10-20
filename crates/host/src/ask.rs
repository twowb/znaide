//! 询问政策:引擎"问用户"的两类问题(工具权限 / 轮数续跑)收在这里。
//!
//! 前端只负责**渲染** [`Ask`],回答一律走 `AgentHandle::answer_permission` /
//! `answer_rounds`。安全语义因此只有一份:
//!
//! - 请求带 `warning`(高危命令 / 目标无法判定)时,`allow_always` 为 `false`,
//!   前端据此禁用"本会话总是允许";
//! - host 在回答入口**再拒一次** —— 前端就算发错了(或将来某个客户端忘了判断),
//!   也放不过去。

use tokio::sync::oneshot;

/// 工具权限请求。
#[derive(Debug, Clone)]
pub struct PermissionAsk {
    pub title: String,
    pub body: String,
    /// `Some(说明)` = 高危/目标无法判定:红色渲染,且不提供"本会话都允许"
    pub warning: Option<String>,
    /// 是否允许"本会话总是允许"这个选项(有 warning 时为 false)
    pub allow_always: bool,
}

/// 轮数预算用尽的续跑询问。
#[derive(Debug, Clone, Copy)]
pub struct RoundsAsk {
    pub used: usize,
}

/// 需要用户回答的询问。
#[derive(Debug, Clone)]
pub enum Ask {
    Permission(PermissionAsk),
    Rounds(RoundsAsk),
}

/// 回答权限询问时套用的政策。
///
/// `allow_always = false`(带 warning 的高危命令)时,即使前端传了 `always = true`,
/// 也只按"单次放行"生效 —— 这是第二把锁,防未来某个客户端漏判。
pub(crate) fn resolve_permission(allow: bool, always: bool, allow_always: bool) -> (bool, bool) {
    (allow, always && allow_always)
}

/// 待回答的询问(只在 host 内部流转)。
pub(crate) enum Pending {
    Permission {
        tx: oneshot::Sender<(bool, bool)>,
        allow_always: bool,
    },
    Rounds {
        tx: oneshot::Sender<bool>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 高危命令:前端误发"总是允许"也只当作单次放行
    #[test]
    fn dangerous_request_never_becomes_always() {
        assert_eq!(resolve_permission(true, true, false), (true, false));
        assert_eq!(resolve_permission(false, true, false), (false, false));
    }

    /// 普通请求:always 照传
    #[test]
    fn normal_request_keeps_always() {
        assert_eq!(resolve_permission(true, true, true), (true, true));
        assert_eq!(resolve_permission(true, false, true), (true, false));
    }
}
