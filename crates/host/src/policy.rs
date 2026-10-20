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

/// 弱网重试的追加尝试次数:`--no-retry` > `--retry N` > config `retry` > 0(关)。
pub fn retry(cli: Option<usize>, no_retry: bool) -> usize {
    super::prepare::effective_retry(&Config::load().unwrap_or_default(), cli, no_retry)
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
        assert_eq!(retry(Some(3), true), 0, "--no-retry 压过 --retry");
        assert_eq!(retry(Some(99), false), znaide_core::config::MAX_RETRY);
    }
}
