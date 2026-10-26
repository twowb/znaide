//! 会话参数政策:轮数上限与重试次数怎么定。
//!
//! 只此一份 —— 以前 CLI、TUI、`/config` 重配各有一份 `effective_*`,改一处漏两处。
//! 规则:命令行覆盖 > 配置文件 > 内置默认。

use znaide_core::config::Config;

/// 单条消息允许的模型往返轮数上限:`--max-turns N` > config `max_turns`(0 = 不限)> 默认。
pub fn max_turns(cli: Option<usize>) -> usize {
    cli.or_else(|| Config::load().ok().map(|c| c.effective_max_turns()))
        .unwrap_or(znaide_core::session::DEFAULT_MAX_TURNS)
}

/// 重试次数的命令行覆盖。启动与 `/config` 重配共用同一份政策 —— 以前 CLI、TUI 启动、
/// TUI 重配各抄了一份,是"改了配置没生效"这类 bug 的温床。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RetryOverride {
    pub cli: Option<usize>,
    pub no_retry: bool,
}

impl RetryOverride {
    /// 按配置算出生效值:`--no-retry` > `--retry N` > config `retry` > 0(关)
    pub fn resolve(&self, cfg: &Config) -> usize {
        if self.no_retry {
            return 0;
        }
        match self.cli {
            Some(n) => n.min(znaide_core::config::MAX_RETRY),
            None => cfg.effective_retry(),
        }
    }

    /// 现读配置文件再算(启动与重配都用这个入口)
    pub fn resolve_now(&self) -> usize {
        self.resolve(&Config::load().unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 命令行 --max-turns 覆盖 config(0 = 不限,不能被换成默认)
    #[test]
    fn cli_max_turns_overrides_config() {
        assert_eq!(max_turns(Some(5)), 5);
        assert_eq!(max_turns(Some(0)), 0);
    }

    /// --no-retry 最高优先;--retry 夹到 0..=MAX_RETRY
    #[test]
    fn retry_override_wins_and_clamps() {
        let off = RetryOverride {
            cli: Some(3),
            no_retry: true,
        };
        assert_eq!(off.resolve_now(), 0, "--no-retry 压过 --retry");
        let clamped = RetryOverride {
            cli: Some(99),
            no_retry: false,
        };
        assert_eq!(clamped.resolve_now(), znaide_core::config::MAX_RETRY);
    }
}
