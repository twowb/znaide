//! 启动准备序列:任何前端(TUI / GUI / 无头)都先走这里,把引擎所需的环境准备好。
//!
//! 只做"与界面无关"的事:数据目录、内置文件与技能、历史清理与迁移、配置读取与格式
//! 迁移、代理与预设请求头的解析、工作目录校验、首启判定、`--resume` 目标定位、
//! 重试次数与轮数上限的落定。
//!
//! **提示语不打印**:统一以 [`Note`] 返回,谁打印(stderr / 界面提示条)、什么时候
//! 打印由前端决定 —— 否则 GUI 会被 CLI 的 stdout 格式绑住。

use std::path::PathBuf;

use anyhow::Result;
use znaide_core::config::{Config, EffectiveProxy, HeaderOverride, ProxyOverride, Resolved};

/// 提示级别。前端据此决定前缀/配色(CLI: ⚠ / 原样;GUI: 黄色 / 灰色 Toast)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteLevel {
    /// 需要用户注意(代理/请求头配置有问题、清理了残留…)
    Warn,
    /// 只是告知(迁移完成、格式升级)
    Info,
}

#[derive(Debug, Clone)]
pub struct Note {
    pub level: NoteLevel,
    pub text: String,
}

impl Note {
    fn warn(text: impl Into<String>) -> Self {
        Self {
            level: NoteLevel::Warn,
            text: text.into(),
        }
    }

    fn info(text: impl Into<String>) -> Self {
        Self {
            level: NoteLevel::Info,
            text: text.into(),
        }
    }
}

/// 前端传进来的启动参数(全是"用户显式给了什么")。
///
/// `config_provided` 由前端按自己的参数面算好再传:CLI 的判定是"配置文件存在 /
/// 命令行给了 provider·model·base_url·api_key / 环境变量齐了";GUI 将来是同构的一套。
/// 这样 host 不必知道 clap 或任何界面形态。
#[derive(Debug, Clone, Default)]
pub struct StartupArgs {
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub provider: Option<String>,
    pub proxy: ProxyOverride,
    pub headers: HeaderOverride,
    pub retry: Option<usize>,
    pub no_retry: bool,
    pub max_turns: Option<usize>,
    /// `--resume` 的原始参数(会话 ID 或文件名片段);`None` = 不恢复。
    /// 无头模式由前端自行忽略(host 不区分模式)。
    pub resume_target: Option<String>,
    /// 是否已有可用配置来源(决定要不要进配置引导)
    pub config_provided: bool,
}

/// 准备结果:前端拿它去开界面/跑无头。
pub struct Prepared {
    /// 读到的配置(已做格式迁移),供后续 `save_*` 用
    pub cfg: Config,
    /// 解析成功 = 可以直接连;`None` = 缺配置(看 `resolve_error`,前端进引导)
    pub resolved: Option<Resolved>,
    pub resolve_error: Option<String>,
    /// 本次生效的代理(fallback 与提示用)
    pub proxy: EffectiveProxy,
    pub persona: String,
    pub cwd: PathBuf,
    pub first_run: bool,
    /// 按会话 ID / 片段定位到的历史会话(`resume_error` 非空时这里是 None)
    pub resume: Option<PathBuf>,
    /// `--resume` 找不到时的文案(前端打印后正常退出,不当成错误退出码)
    pub resume_error: Option<String>,
    /// 生效的重试次数(`--no-retry` > `--retry N` > 配置 > 0)
    pub retry: usize,
    /// 命令行 `--max-turns` 覆盖(原样透传;None = 配置/默认)
    pub max_turns: Option<usize>,
    pub notes: Vec<Note>,
}

impl Prepared {
    /// 配置解析失败时给界面撑住用的占位端点(进配置引导,真正连接前会被覆盖)。
    pub fn fallback_resolved(&self) -> Resolved {
        Resolved {
            model: String::new(),
            base_url: "http://localhost:11434/v1".into(),
            api_key: None,
            provider_name: "ollama".into(),
            context_window: None,
            proxy: self.proxy.clone(),
            headers: Default::default(),
        }
    }
}

/// 跑一遍启动准备序列。顺序即语义,别随手调换(清理/迁移要在读配置之前)。
pub fn prepare(args: StartupArgs) -> Result<Prepared> {
    let mut notes = Vec::new();

    // 数据目录 + 内置资源(缺失才写,不覆盖用户改动)+ 历史清理/迁移
    znaide_core::config::ensure_data_dirs()?;
    znaide_core::persona::ensure_builtin_files();
    znaide_core::skills::ensure_builtin_skills();
    let pruned = znaide_core::session::prune_empty_sessions();
    if pruned > 0 {
        notes.push(Note::info(format!(
            "♻ 已清理 {pruned} 个空的会话文件(未产生任何对话的残留)"
        )));
    }
    let migrated = znaide_core::skills::migrate_legacy_commands();
    if migrated > 0 {
        notes.push(Note::info(format!(
            "♻ 已将 {migrated} 个旧自定义命令迁移为 skill(~/.znaide/skills/,保持仅手动触发)"
        )));
    }

    let mut cfg = Config::load()?;
    // 配置格式标记迁移:他版/外部写入的 build_tag 会被自动置回当前版本
    if cfg.migrate()? {
        notes.push(Note::info("♻ 检测到配置由其他版本写入,格式标记已升级。"));
    }
    let persona = cfg.persona.clone().unwrap_or_default();

    // 代理:配置文件 → 专用环境变量 → 命令行(命令行那层最高)
    let (proxy, proxy_notes) = cfg.resolve_proxy(&args.proxy);
    notes.extend(proxy_notes.into_iter().map(Note::warn));

    let mut resolved = cfg.resolve(
        args.model.clone(),
        args.base_url.clone(),
        args.api_key.clone(),
        args.provider.clone(),
    );
    let mut resolve_error = None;
    match resolved.as_mut() {
        Ok(r) => {
            r.proxy = proxy.clone();
            // 预设请求头:配置 + 专用环境变量在 `resolve` 里已落定,这里叠命令行那层
            let (headers, header_notes) = cfg.resolve_headers(&r.provider_name, &args.headers);
            notes.extend(header_notes.into_iter().map(Note::warn));
            r.headers = headers;
        }
        Err(e) => resolve_error = Some(format!("{e:#}")),
    }

    let cwd = args.cwd;
    if !cwd.is_dir() {
        anyhow::bail!("工作目录不存在: {}", cwd.display());
    }

    // `--resume` 只在交互模式有意义;无头由前端传 None
    let (resume, resume_error) = match args.resume_target.as_deref() {
        Some(target) => match resolve_resume(target) {
            Ok(p) => (Some(p), None),
            // 找不到会话不是"程序出错":前端打印这句就正常退出
            Err(e) => (None, Some(format!("{e}"))),
        },
        None => (None, None),
    };

    let retry = effective_retry(&cfg, args.retry, args.no_retry);

    Ok(Prepared {
        cfg,
        resolved: resolved.ok(),
        resolve_error,
        proxy,
        persona,
        cwd,
        first_run: !args.config_provided,
        resume,
        resume_error,
        retry,
        max_turns: args.max_turns,
        notes,
    })
}

/// 弱网重试的追加尝试次数:`--no-retry` > `--retry N` > 配置 > 0(关)。
/// 启动与将来的"重配"都走这里 —— 政策只有一份,别在前端再抄一遍。
pub fn effective_retry(cfg: &Config, cli_retry: Option<usize>, no_retry: bool) -> usize {
    if no_retry {
        return 0;
    }
    match cli_retry {
        Some(n) => n.min(znaide_core::config::MAX_RETRY),
        None => cfg.effective_retry(),
    }
}

/// 按会话 ID(文件名 stem)或文件名片段定位历史会话;无匹配时列出最近的供参考。
/// 会话列表的 `[无头]` 标记也在文案里说明 —— 这句文案以前在 CLI 里,GUI 也需要同一份。
fn resolve_resume(target: &str) -> Result<PathBuf> {
    let sessions = znaide_core::session::list_history_sessions_detailed();
    if let Some(h) = sessions.iter().find(|h| {
        h.path
            .file_stem()
            .map(|s| s.to_string_lossy() == target)
            .unwrap_or(false)
    }) {
        return Ok(h.path.clone());
    }
    if let Some(h) = sessions.iter().find(|h| {
        h.path.to_string_lossy().contains(target)
            || h.path
                .file_stem()
                .map(|s| s.to_string_lossy().contains(target))
                .unwrap_or(false)
    }) {
        return Ok(h.path.clone());
    }
    let mut msg = format!(
        "未找到匹配「{target}」的历史会话。最近 {} 个会话(ID 即文件名,[无头]=命令行 -p 产生):\n",
        sessions.len().min(8)
    );
    for h in sessions.iter().take(8) {
        let stem = h
            .path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let flag = if h.headless { " [无头]" } else { "" };
        msg.push_str(&format!("  {stem}{flag}\n"));
    }
    anyhow::bail!(msg.trim_end().to_string())
}
