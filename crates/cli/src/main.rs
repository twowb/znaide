use clap::Parser;
use tokio_util::sync::CancellationToken;
use znaide_core::config::{Config, Resolved};
use znaide_core::llm::OpenAiClient;
use znaide_core::permissions::Mode;
use znaide_core::session::Session;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "znaide",
    version,
    about = "无所不能的终端 AI 助手",
    long_about = "znaide:跑在终端里的通用 AI 助手。用自然语言下达指令,它自主完成:改文件、跑命令、查资料……\n\n用法示例:\n  znaide                              # 交互模式(终端对话)\n  znaide -p \"把 ~/Downloads 里的 zip 按日期归档\"  # 无头模式执行任务\n\n配置:编辑 ~/.znaide/config.json,或用环境变量 ZNAIDE_MODEL / ZNAIDE_BASE_URL / ZNAIDE_API_KEY(兼容 OPENAI_* 系列)。"
)]
struct Cli {
    /// 无头模式:把指令交给 agent 自主执行
    #[arg(short = 'p', long = "print", value_name = "PROMPT")]
    prompt: Option<String>,

    /// 权限模式:ask / acceptEdits / bypassPermissions / yolo(默认 ask)
    #[arg(long, value_name = "MODE", default_value = "ask")]
    permission: String,

    /// 工作目录(缺省为当前目录)
    #[arg(long, value_name = "DIR")]
    cwd: Option<PathBuf>,

    /// 覆盖模型名
    #[arg(long, value_name = "MODEL")]
    model: Option<String>,

    /// 覆盖 OpenAI 兼容端点
    #[arg(long, value_name = "URL")]
    base_url: Option<String>,

    /// 覆盖 API key
    #[arg(long, value_name = "KEY")]
    api_key: Option<String>,

    /// 选择 provider 预设(ollama/dashscope/deepseek/…,可在 config.json 的 providers 自定义)
    #[arg(long, value_name = "NAME")]
    provider: Option<String>,

    /// 恢复指定历史会话(会话 ID 或文件名片段;ID 见底部状态栏/退出统计)
    #[arg(long, value_name = "TARGET")]
    resume: Option<String>,

    /// 检查并安装 GitHub 最新版本(见 --version 查看当前版本)
    #[arg(long)]
    update: bool,
}

/// 执行更新(--update):探测最新版 → 下载 → 自检 → 安装
async fn run_update() -> anyhow::Result<()> {
    use znaide_core::update::UpdateResult;
    let cur = znaide_core::update::current_version();
    println!("当前版本: v{cur}");
    match znaide_core::update::perform_update().await {
        UpdateResult::UpToDate => println!("已是最新版本 v{cur}。"),
        UpdateResult::Updated { version, source, deferred: false } => {
            println!("✔ 已更新到 v{version}(来源 {source})。下次启动即生效(本次会话继续用旧版本)。");
        }
        UpdateResult::Updated { version, source, deferred: true } => {
            println!("✔ 新版本 v{version}(来源 {source})已就位:退出本程序后会自动完成替换,下次启动即生效。");
        }
        UpdateResult::CheckFailed(e) => eprintln!("⚠ 检查更新失败: {e}"),
        UpdateResult::DownloadFailed(e) => eprintln!("⚠ 更新失败: {e}"),
        UpdateResult::VerifyFailed(e) => eprintln!("⚠ {e}"),
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // --update 与其它参数独立:不需要任何配置/会话
    if cli.update {
        return run_update().await;
    }

    let mode = Mode::parse(&cli.permission)?;

    // 建数据目录;顺带清掉历史遗留的空会话文件、迁移旧版 commands/*.md 为 skill、
    // 把内置人格同步成 ~/.znaide/personas/ 的可编辑副本(缺失才写,不覆盖用户改动)、
    // 物化内置示例技能到 ~/.znaide/builtin-skills/(开箱即用,不再单独发 skills 包)
    let _ = znaide_core::config::ensure_data_dirs()?;
    znaide_core::persona::ensure_builtin_files();
    znaide_core::skills::ensure_builtin_skills();
    let pruned = znaide_core::session::prune_empty_sessions();
    if pruned > 0 {
        eprintln!("♻ 已清理 {pruned} 个空的会话文件(未产生任何对话的残留)");
    }
    let migrated = znaide_core::skills::migrate_legacy_commands();
    if migrated > 0 {
        eprintln!("♻ 已将 {migrated} 个旧自定义命令迁移为 skill(~/.znaide/skills/,保持仅手动触发)");
    }

    let mut cfg = Config::load()?;
    // 配置格式标记迁移:他版/外部写入的 build_tag 会被自动置回当前版本
    if cfg.migrate()? {
        eprintln!("♻ 检测到配置由其他版本写入,格式标记已升级。");
    }
    // 全局人格(空 = 不注入),随配置持久
    let persona = cfg.persona.clone().unwrap_or_default();
    let resolved = cfg.resolve(
        cli.model.clone(),
        cli.base_url.clone(),
        cli.api_key.clone(),
        cli.provider.clone(),
    );
    let cwd = cli.cwd.clone().unwrap_or(std::env::current_dir()?);
    if !cwd.is_dir() {
        anyhow::bail!("工作目录不存在: {}", cwd.display());
    }

    match cli.prompt.as_deref() {
        Some(prompt) => {
            if cli.resume.is_some() {
                eprintln!("⚠ --resume 仅交互模式可用;已忽略(-p 无头模式)。");
            }
            // 什么配置来源都没有时才引导
            let has_config = znaide_core::config::config_exists()
                || cli.model.is_some()
                || cli.base_url.is_some()
                || cli.api_key.is_some()
                || cli.provider.is_some()
                || std::env::var("ZNAIDE_MODEL").is_ok()
                || std::env::var("OPENAI_MODEL").is_ok()
                || std::env::var("ZNAIDE_BASE_URL").is_ok()
                || std::env::var("OPENAI_BASE_URL").is_ok();
            if !has_config {
                print_first_run_headless_hint();
                return Ok(());
            }
            let resolved = resolved?;
            run_headless(&resolved, mode, &cwd, prompt, &persona).await?;
        }
        None => {
            // 首次运行(无配置)自动进配置引导
            let first_run = !znaide_core::config::config_exists();
            let resolved = resolved.unwrap_or_else(|e| {
                eprintln!("⚠ 配置解析: {e}");
                // 先给个默认值撑住引导界面,真正连接前会被覆盖
                znaide_core::config::Resolved {
                    model: String::new(),
                    base_url: "http://localhost:11434/v1".into(),
                    api_key: None,
                    provider_name: "ollama".into(),
                    context_window: None,
                }
            });
            // --resume:按 ID/文件名片段定位历史文件,启动即恢复
            let resume = match cli.resume.as_deref() {
                Some(target) => match resolve_resume(target) {
                    Ok(p) => {
                        eprintln!(
                            "▶ 恢复会话 {}…(退出:输入 /quit、/exit 或 Ctrl+C)",
                            p.file_stem()
                                .map(|s| s.to_string_lossy().to_string())
                                .unwrap_or_default()
                        );
                        Some(p)
                    }
                    Err(e) => {
                        eprintln!("{e}");
                        return Ok(());
                    }
                },
                None => None,
            };
            let stats = run_interactive(&resolved, mode, &cwd, first_run, resume, persona.clone()).await?;
            print_session_stats(&stats);
        }
    }
    Ok(())
}

/// 按会话 ID(文件名 stem)或文件名片段定位历史会话文件;无匹配时列出最近的供参考
fn resolve_resume(target: &str) -> anyhow::Result<PathBuf> {
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
    let mut msg = format!("未找到匹配「{target}」的历史会话。最近 {} 个会话(ID 即文件名,[无头]=命令行 -p 产生):\n", sessions.len().min(8));
    for h in sessions.iter().take(8) {
        let stem = h.path.file_stem().unwrap_or_default().to_string_lossy().to_string();
        let flag = if h.headless { " [无头]" } else { "" };
        msg.push_str(&format!("  {stem}{flag}\n"));
    }
    let msg = msg.trim_end().to_string();
    anyhow::bail!(msg)
}

/// 会话结束统计的格式化时长(秒 → 人类可读)
fn fmt_duration(secs: u64) -> String {
    if secs >= 3600 {
        format!("{} 小时 {} 分", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{} 分 {} 秒", secs / 60, secs % 60)
    } else {
        format!("{secs} 秒")
    }
}

/// 打印本次交互会话的统计(退出 TUI 后回到普通终端)
fn print_session_stats(s: &znaide_tui::ExitStats) {
    println!("\n═══════ 本次会话统计 ═══════");
    println!(
        "会话时长:{} · 会话 ID:{}",
        fmt_duration(s.uptime_secs),
        if s.session_id.is_empty() { "-" } else { &s.session_id }
    );
    println!(
        "消息:    用户 {} 条 · 助手 {} 条",
        s.user_msgs, s.assistant_msgs
    );
    println!(
        "工具:    共 {} 次(shell 命令 {} · 文件写/技能 {})",
        s.tool_calls, s.shell_calls, s.file_writes
    );
    println!(
        "Token:   输入 {} · 输出 {} · 合计 {}(服务端真实 usage)",
        s.tokens_in,
        s.tokens_out,
        s.tokens_in + s.tokens_out
    );
    println!("undo:    本次改动 {} 个文件快照", s.undo_snapshots);
    if !s.session_id.is_empty() && s.history.is_some() {
        println!(
            "继续:   下次运行 `znaide --resume {}` 可回到本会话继续对话",
            s.session_id
        );
    } else if s.user_msgs == 0 && s.assistant_msgs == 0 {
        println!("历史:    (本次会话没有对话内容,未生成历史文件)");
    }
    println!("════════════════════════════");
}

/// 无头模式首次运行提示
fn print_first_run_headless_hint() {
    eprintln!(
        "\n还没有配置任何模型服务。请先运行一次交互模式完成配置向导:\n\n  znaide\n\n\
         或直接用命令行指定(推荐先交互配置一次):\n\n  znaide --provider deepseek -p \"你好\"\n  znaide --model qwen3:8b --base-url http://localhost:11434/v1 -p \"你好\"\n\n\
         配置会保存在 {}。\n",
        znaide_core::config::config_path().display()
    );
}

async fn run_headless(
    resolved: &Resolved,
    mode: Mode,
    cwd: &PathBuf,
    prompt: &str,
    persona: &str,
) -> anyhow::Result<()> {
    let llm = OpenAiClient::new(resolved)?;

    // MCP(仅当 ~/.znaide/mcp.json 存在)
    let mcp_cfg_path = znaide_core::config::data_dir().join("mcp.json");
    let mcp = if mcp_cfg_path.exists() {
        let m = znaide_core::mcp::McpManager::start().await;
        for (name, err) in &m.errors {
            eprintln!("⚠ MCP: {name}: {err}");
        }
        if !m.client_names().is_empty() {
            eprintln!("🔌 MCP servers: {}", m.client_names().join(", "));
        }
        Some(m)
    } else {
        None
    };

    let mode_cn = match mode {
        Mode::Ask => "询问",
        Mode::AcceptEdits => "编辑放行",
        Mode::BypassPermissions => "全自动",
        Mode::Yolo => "超级(YOLO)",
    };
    eprintln!("▶ 正在执行(模型: {},权限: {mode_cn})…", resolved.model);
    let mut session = Session::new(
        llm,
        cwd.clone(),
        mode,
        None,
        CancellationToken::new(),
        true,
        None,
        mcp,
        persona,
    )?;
    let result = session
        .run_turn(prompt)
        .await
        .map_err(|e| anyhow::anyhow!("{}", znaide_core::session::describe_request_error(&e)))?;

    if result.truncated && result.tool_calls > 0 {
        eprintln!("\n⚠ 达到工具调用轮数上限,任务可能未完成。");
    }
    println!("\n{}", result.text);
    if result.tool_calls > 0 || result.input_tokens > 0 || result.output_tokens > 0 {
        eprintln!(
            "(共调用 {} 次工具,输入 {} / 输出 {} tokens;会话 {} 历史:{})",
            result.tool_calls,
            result.input_tokens,
            result.output_tokens,
            session.session_id(),
            session.history_path().map(|p| p.display().to_string()).unwrap_or_default()
        );
    }
    Ok(())
}

async fn run_interactive(
    resolved: &Resolved,
    mode: Mode,
    cwd: &PathBuf,
    first_run: bool,
    resume: Option<PathBuf>,
    persona: String,
) -> anyhow::Result<znaide_tui::ExitStats> {
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        anyhow::bail!("交互模式需要终端。请在终端中运行 znaide,或用 -p \"指令\" 使用无头模式。");
    }
    let stats = znaide_tui::run(resolved, mode, cwd, first_run, resume, persona).await?;
    Ok(stats)
}
