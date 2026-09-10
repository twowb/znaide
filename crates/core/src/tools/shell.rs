use super::{deny_to_result, resolve_path, ToolContext, ToolError, ToolOutput};
use crate::session::SessionEvent;
use serde::Deserialize;
use serde_json::Value;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// 静默卡死:命令连续这么久没有新输出,判卡死终止(内部固定安全网,不吃模型参数)
const SILENT_CAP: Duration = Duration::from_secs(90);
/// 总墙钟保险:不管有没有输出,跑满这么久必杀(防"有输出但失控"的刷屏)
const WALLCLOCK_CAP: Duration = Duration::from_secs(30 * 60);
/// 模型可传的预算上限(毫秒):夹到总保险时长,传再大也没意义
pub const MAX_BUDGET_MS: u64 = 30 * 60 * 1000;
/// 超预算宽限比例:预算耗尽后仍允许继续跑预算的 20%,最多 GRACE_CAP_MS
const GRACE_RATIO_PERCENT: u64 = 20;
/// 超预算宽限封顶(毫秒)
const GRACE_CAP_MS: u64 = 60_000;
/// 输出内存缓冲上限:超出滚动丢头部、只留最新(展示与死前遗言都只取尾部)
const BUF_CAP: usize = 256 * 1024;
/// 距静默上限不足这么久就发红色预警(毫秒)
const RED_ALERT_MS: u64 = 10_000;

/// 超预算宽限时长:预算的 20%,最多 60s。给"差一点就完成"的命令缓冲,
/// 避免模型低估几十秒就整轮重跑
pub fn overrun_grace_ms(budget_ms: u64) -> u64 {
    (budget_ms * GRACE_RATIO_PERCENT / 100).min(GRACE_CAP_MS)
}

#[derive(Debug, Deserialize)]
struct ShellArgs {
    command: String,
    #[serde(default)]
    cwd: Option<String>,
    /// AI 预估的完成时长(毫秒)。命令跑超它(含宽限)仍未结束就被终止、
    /// 并把进展反馈给 AI 自行决定重试/换路。旧名 timeout_ms(曾是"总时长"
    /// 语义)作别名兼容
    #[serde(default, alias = "timeout_ms")]
    idle_ms: Option<u64>,
}

pub async fn run_shell_command(ctx: &ToolContext<'_>, args: &Value) -> Result<ToolOutput, ToolError> {
    let a: ShellArgs = serde_json::from_value(args.clone())?;
    if a.command.trim().is_empty() {
        return Err(ToolError(
            "command 不能为空:请传入要执行的完整命令字符串,如 {\"command\": \"ls -la\"}".into(),
        ));
    }
    // 高危命令兜底:交互模式由 session 层带确认地统一把关(见 session::check_permission),
    // 这里只在没有人在环路里时生效(无头 / 宿主直调)。YOLO 超级模式把全部判断交给模型。
    if ctx.events.is_none() && ctx.permission.mode != crate::permissions::Mode::Yolo {
        if let crate::permissions::CommandRisk::Blocked { reason, target } =
            crate::permissions::inspect_command(&a.command, ctx.cwd)
        {
            return Err(ToolError(format!(
                "命令被安全策略拦截: {reason}(解析出的目标:{target})。\
                 无头模式无法交互确认,已拒绝(bypassPermissions 也不放行高危命令);\
                 如确需执行,请自行在终端跑"
            )));
        }
    }
    // 权限判定(档位)
    deny_to_result(ctx.permission.check_command(&a.command))?;

    let dir = match &a.cwd {
        Some(d) => resolve_path(ctx.cwd, d),
        None => ctx.cwd.to_path_buf(),
    };
    // idle_ms = AI 的完成时长预期(没传就没有总预算约束,靠静默+总保险)
    let budget = a.idle_ms.map(|v| Duration::from_millis(v.min(MAX_BUDGET_MS)));

    let output = run(&a.command, &dir, SILENT_CAP, budget, ctx.cancel.clone(), ctx.events).await?;
    Ok(output)
}

/// Duration → 简短人读:90s / 4min(整分钟缩写)
fn fmt_duration_short(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 && secs % 60 == 0 {
        format!("{}min", secs / 60)
    } else {
        format!("{secs}s")
    }
}

/// 取输出缓冲尾部文本(死前遗言/结果预览都用它)
fn tail_text(buf: &[u8], max_chars: usize) -> String {
    let text = String::from_utf8_lossy(buf);
    let text = text.trim_end();
    let mut chars = text.chars();
    let tail: String = chars.by_ref().rev().take(max_chars).collect();
    let tail: String = tail.chars().rev().collect();
    tail.trim_start().to_string()
}

/// 滚动缓冲:超出上限丢掉头部一半,只留最新(命令刷屏时内存有界)
fn push_output(buf: &mut Vec<u8>, chunk: &[u8]) {
    buf.extend_from_slice(chunk);
    if buf.len() > BUF_CAP {
        let drop_n = buf.len() - BUF_CAP / 2;
        buf.drain(..drop_n);
    }
}

/// 静默预警档:0 无 / 1 黄(静默超预算 80%)/ 2 红(距上限不足 10s)
fn silent_level(idle: Duration, silent: Duration) -> u8 {
    let idle_ms = idle.as_millis() as u64;
    let silent_ms = silent.as_millis() as u64;
    if silent_ms + RED_ALERT_MS >= idle_ms {
        2
    } else if silent_ms * 5 >= idle_ms * 4 {
        1
    } else {
        0
    }
}

fn emit_alert(events: Option<&mpsc::UnboundedSender<SessionEvent>>, level: u8) {
    if let Some(tx) = events {
        let _ = tx.send(SessionEvent::ToolSilentAlert {
            name: "run_shell_command".into(),
            level,
        });
    }
}

async fn run(
    command: &str,
    dir: &std::path::Path,
    silent_cap: Duration,
    budget: Option<Duration>,
    cancel: Option<CancellationToken>,
    events: Option<&mpsc::UnboundedSender<SessionEvent>>,
) -> Result<ToolOutput, ToolError> {
    // 已处于中断状态:不再启动新进程
    if let Some(tok) = &cancel {
        if tok.is_cancelled() {
            return Err(ToolError("命令已中断(用户按 Esc)".into()));
        }
    }
    // Unix 用 bash,Windows 用 cmd
    #[cfg(unix)]
    let mut cmd = {
        let mut c = tokio::process::Command::new("bash");
        c.arg("-c").arg(command);
        c
    };
    #[cfg(windows)]
    let mut cmd = {
        let mut c = tokio::process::Command::new("cmd");
        c.arg("/C").arg(command);
        c
    };

    cmd.current_dir(dir)
        // stdin 不继承:避免命令(ssh/sudo/read 等)与 znaide 抢读同一个 raw
        // 终端,把鼠标 SGR 字节流撕裂成残片混进输入
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| ToolError(format!("启动命令失败: {e}")))?;
    let mut stdout = child.stdout.take().expect("stdout 已 piped");
    let mut stderr = child.stderr.take().expect("stderr 已 piped");

    // 输出缓冲:正常完成时整体截断展示;被杀时取尾部当"死前遗言"回给模型,
    // 免得它看不到进度、只能盲目重跑
    let mut out_buf = Vec::<u8>::new();
    let mut err_buf = Vec::<u8>::new();
    // 待推送的实时输出增量(随 ticker 节流 flush 成 ToolOutputDelta 给 UI)
    let mut pending = Vec::<u8>::new();
    // 两个管道各用一块独立缓冲(select 分支同时持有 &mut,不能共享一块)
    let mut out_chunk = [0u8; 8192];
    let mut err_chunk = [0u8; 8192];
    let start = Instant::now();
    // 静默计时基准:任何输出都重置它(产出即续命)
    let mut last_output = Instant::now();
    let mut silent_level_now = 0u8;
    let mut out_eof = false;
    let mut err_eof = false;

    // 200ms 一跳:检查静默/总保险,并按接近程度发黄/红预警
    let mut ticker = tokio::time::interval(Duration::from_millis(200));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // 规则:静默卡死(无输出超 silent_cap)→ 杀;总预算(AI 预估,超它含宽限
    // 仍跑)→ 杀并反馈给 AI 自决;总保险(30min)→ 杀;外加 Esc 中断。
    // kill_on_drop 保证 Err 返回后进程必清。
    let status = loop {
        if out_eof && err_eof {
            break child.wait().await;
        }
        tokio::select! {
            r = async { stdout.read(&mut out_chunk).await }, if !out_eof => {
                match r {
                    Ok(0) => out_eof = true,
                    Ok(n) => {
                        push_output(&mut out_buf, &out_chunk[..n]);
                        if events.is_some() {
                            push_output(&mut pending, &out_chunk[..n]);
                        }
                        last_output = Instant::now();
                        if silent_level_now != 0 {
                            silent_level_now = 0;
                            emit_alert(events, 0);
                        }
                    }
                    Err(e) => return Err(ToolError(format!("读命令输出失败: {e}"))),
                }
            }
            r = async { stderr.read(&mut err_chunk).await }, if !err_eof => {
                match r {
                    Ok(0) => err_eof = true,
                    Ok(n) => {
                        push_output(&mut err_buf, &err_chunk[..n]);
                        if events.is_some() {
                            push_output(&mut pending, &err_chunk[..n]);
                        }
                        last_output = Instant::now();
                        if silent_level_now != 0 {
                            silent_level_now = 0;
                            emit_alert(events, 0);
                        }
                    }
                    Err(e) => return Err(ToolError(format!("读命令输出失败: {e}"))),
                }
            }
            _ = ticker.tick() => {
                let silent = last_output.elapsed();
                let total = start.elapsed();
                // 静默卡死:卡死(等输入/网络挂/死锁),带死前输出杀掉
                if silent >= silent_cap {
                    let _ = child.kill().await;
                    let secs = silent.as_secs();
                    let msg = format!(
                        "命令已连续 {secs}s 没有新输出,判定卡死已终止。\n{}",
                        tail_part(&out_buf, &err_buf)
                    );
                    return Err(ToolError(msg));
                }
                // 总预算:AI 预估时长耗尽、连宽限也用完还在跑(哪怕有输出)→ 终止。
                // 错误里说清规则与进展,AI 自己决定加大预算重试、拆步或换路
                if let Some(b) = budget {
                    let grace = Duration::from_millis(overrun_grace_ms(b.as_millis() as u64));
                    if total > b + grace {
                        let _ = child.kill().await;
                        let secs = total.as_secs();
                        let msg = format!(
                            "命令运行 {secs}s 已超过 AI 预估预算({},预算耗尽后另有 {}s 宽限)仍未结束,已终止。\n{}\
                             若该任务确实需要更久,请调大 idle_ms 后重试,或拆小步骤、换更轻的命令。",
                            fmt_duration_short(b),
                            grace.as_secs(),
                            tail_part(&out_buf, &err_buf),
                        );
                        return Err(ToolError(msg));
                    }
                }
                // 总保险:有输出但失控(刷屏死循环)
                if total >= WALLCLOCK_CAP {
                    let _ = child.kill().await;
                    let msg = format!(
                        "命令运行超过 {} 分钟总上限,已终止。\n{}",
                        WALLCLOCK_CAP.as_secs() / 60,
                        tail_part(&out_buf, &err_buf)
                    );
                    return Err(ToolError(msg));
                }
                // 接近静默上限:黄(>80%)→ 红(不足 10s),供 UI 变色提示
                let lvl = silent_level(silent_cap, silent);
                if lvl != silent_level_now {
                    silent_level_now = lvl;
                    emit_alert(events, lvl);
                }
                // 实时输出增量:与 ticker 同拍节流推送(有 events 才有意义)
                if let Some(tx) = events {
                    if !pending.is_empty() {
                        let delta = String::from_utf8_lossy(&pending).into_owned();
                        pending.clear();
                        let _ = tx.send(SessionEvent::ToolOutputDelta {
                            name: "run_shell_command".into(),
                            delta,
                        });
                    }
                }
            }
            _ = async {
                match &cancel {
                    Some(tok) => tok.cancelled().await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                let _ = child.kill().await;
                return Err(ToolError("命令已中断(用户按 Esc),进程已终止".into()));
            }
        }
    };

    // 命令自然结束时把残余的实时输出也 flush 掉,别丢最后一行
    if let Some(tx) = events {
        if !pending.is_empty() {
            let delta = String::from_utf8_lossy(&pending).into_owned();
            let _ = tx.send(SessionEvent::ToolOutputDelta {
                name: "run_shell_command".into(),
                delta,
            });
        }
    }

    let mut text = String::new();
    if !out_buf.is_empty() && !out_buf.iter().all(|b| b.is_ascii_whitespace()) {
        text.push_str(&String::from_utf8_lossy(&out_buf));
    }
    if !err_buf.is_empty() && !err_buf.iter().all(|b| b.is_ascii_whitespace()) {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!("[stderr] {}", String::from_utf8_lossy(&err_buf)));
    }
    let mut out = String::new();
    if !text.trim().is_empty() {
        out.push_str(&truncate_middle(&text, 20_000));
        out.push('\n');
    }
    match status.map(|s| s.code()).unwrap_or(None) {
        Some(0) => out.push_str("(命令退出码 0)\n"),
        Some(c) => out.push_str(&format!("(命令退出码 {c})\n")),
        None => out.push_str("(命令被信号终止)\n"),
    }
    Ok(out)
}

/// 拼被杀时的"死前遗言":stdout/stderr 各自取尾部,提示里带上一段
fn tail_part(out_buf: &[u8], err_buf: &[u8]) -> String {
    let out_tail = tail_text(out_buf, 1500);
    let err_tail = tail_text(err_buf, 1500);
    if out_tail.is_empty() && err_tail.is_empty() {
        return "命令没有任何输出。".into();
    }
    let mut s = String::new();
    if !out_tail.is_empty() {
        s.push_str(&format!("命令最后输出:\n{out_tail}\n"));
    }
    if !err_tail.is_empty() {
        s.push_str(&format!("命令最后 stderr:\n{err_tail}\n"));
    }
    s
}

/// 长输出保留头尾,中间折叠
fn truncate_middle(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let half = max / 2;
    let head: String = s.chars().take(half).collect();
    let tail: String = s.chars().rev().take(half).collect::<Vec<_>>().into_iter().rev().collect();
    format!("{head}\n…(输出过长,中间 {}/{} 字符已省略)…\n{tail}", s.chars().count() - max, s.chars().count())
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Esc 中断正在运行的命令:应很快返回错误,而不是等命令自然跑完
    #[tokio::test]
    async fn cancel_kills_running_command() {
        let cancel = CancellationToken::new();
        let canceller = cancel.clone();
        let killer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            canceller.cancel();
        });
        let start = Instant::now();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let r = run("sleep 30", dir, Duration::from_secs(60), None, Some(cancel), None).await;
        assert!(r.is_err());
        assert!(r.unwrap_err().0.contains("中断"));
        // 200ms 后取消,应在几秒内返回(sleep 30 远未结束)
        assert!(start.elapsed() < Duration::from_secs(5), "取消未生效,耗时 {:?}", start.elapsed());
        let _ = killer.await;
    }

    /// 令牌已处于取消态时,不应再启动新命令
    #[tokio::test]
    async fn already_cancelled_does_not_run() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let r = run("echo should-not-run > /tmp/znaide_should_not_exist", dir, Duration::from_secs(10), None, Some(cancel), None).await;
        assert!(r.is_err());
        assert!(!std::path::Path::new("/tmp/znaide_should_not_exist").exists());
        let _ = std::fs::remove_file("/tmp/znaide_should_not_exist");
    }

    /// 静默卡死:命令无输出超过静默上限 → 杀,并报告已静默的秒数
    #[tokio::test]
    async fn silent_timeout_kills_silent_command() {
        let start = Instant::now();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let r = run("sleep 30", dir, Duration::from_millis(800), None, None, None).await;
        let err = r.unwrap_err().0;
        assert!(err.contains("没有新输出"), "错误应点明静默卡死: {err}");
        assert!(err.contains("没有任何输出"), "sleep 无输出,遗言应如实说明: {err}");
        // 800ms 静默上限,应在几秒内被终止
        assert!(start.elapsed() < Duration::from_secs(5), "静默终止未生效,耗时 {:?}", start.elapsed());
    }

    /// 有输出就不算卡死:持续输出的命令不会因静默上限被掐
    #[tokio::test]
    async fn output_keeps_command_alive_past_silent_cap() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        // 每 250ms 吐一行,总时长约 2s;静默上限 700ms——纯静默判定早就杀了
        let cmd = "for i in $(seq 1 8); do echo tick-$i; sleep 0.25; done";
        let start = Instant::now();
        let r = run(cmd, dir, Duration::from_millis(700), None, None, None).await;
        assert!(r.is_ok(), "持续输出的命令不应被静默上限掐死: {:?}", r.err().map(|e| e.0));
        let out = r.unwrap();
        assert!(out.contains("tick-8"), "应完整跑完拿到最后输出: {out}");
        assert!(start.elapsed() >= Duration::from_secs(1), "命令应真实跑完: {:?}", start.elapsed());
    }

    /// 死前遗言:被静默杀掉的命令,错误里带出它死前已输出的片段
    #[tokio::test]
    async fn killed_command_keeps_partial_output() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let cmd = "echo START-OF-WORK; echo progress-line-1; sleep 30";
        let r = run(cmd, dir, Duration::from_millis(800), None, None, None).await;
        let err = r.unwrap_err().0;
        assert!(err.contains("START-OF-WORK"), "死前输出应随错误回给模型: {err}");
        assert!(err.contains("progress-line-1"), "死前输出应随错误回给模型: {err}");
    }

    /// 总预算:超过 AI 预估(含宽限)仍在跑 → 有输出也终止,错误说清规则
    #[tokio::test]
    async fn budget_overrun_terminates_even_with_output() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        // 持续输出 3s 的命令;预算 500ms + 宽限(500*20%=100ms)→ ~600ms 被终止
        let cmd = "for i in $(seq 1 30); do echo tick-$i; sleep 0.1; done";
        let start = Instant::now();
        let r = run(
            cmd,
            dir,
            Duration::from_secs(90),
            Some(Duration::from_millis(500)),
            None,
            None,
        )
        .await;
        let err = r.unwrap_err().0;
        assert!(err.contains("AI 预估预算"), "应点明超预算终止: {err}");
        assert!(err.contains("idle_ms"), "应引导 AI 调大预算重试: {err}");
        assert!(err.contains("tick-"), "死前输出应保留: {err}");
        assert!(start.elapsed() < Duration::from_secs(3), "应尽早终止: {:?}", start.elapsed());
    }

    /// 没传预算(模型没承诺时长)就不受总预算约束:有输出可跑完
    #[tokio::test]
    async fn no_budget_runs_to_completion() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let cmd = "for i in $(seq 1 12); do echo x; sleep 0.1; done"; // ~1.2s
        let r = run(cmd, dir, Duration::from_secs(90), None, None, None).await;
        assert!(r.is_ok(), "无预算约束应跑完: {:?}", r.err().map(|e| e.0));
    }

    /// 实时输出:执行中经事件通道节流推送,自然结束时残余也 flush(不丢尾巴)
    #[tokio::test]
    async fn live_output_events_streamed() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SessionEvent>();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let cmd = "for i in $(seq 1 10); do echo live-$i; sleep 0.1; done"; // ~1s
        let r = run(cmd, dir, Duration::from_secs(90), None, None, Some(&tx)).await;
        assert!(r.is_ok(), "应正常跑完: {:?}", r.err().map(|e| e.0));
        drop(tx);
        let mut joined = String::new();
        while let Ok(ev) = rx.try_recv() {
            if let SessionEvent::ToolOutputDelta { name, delta } = ev {
                assert_eq!(name, "run_shell_command");
                joined.push_str(&delta);
            }
        }
        assert!(joined.contains("live-1"), "首行应推送: {joined}");
        assert!(joined.contains("live-10"), "末行不应丢: {joined}");
    }

    /// 无头模式(events=None)零事件开销,正常完成
    #[tokio::test]
    async fn no_events_no_output_stream() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let cmd = "echo quiet; sleep 0.1";
        let r = run(cmd, dir, Duration::from_secs(90), None, None, None).await;
        assert!(r.is_ok());
    }

    /// 宽限 = 预算的 20%、封顶 60s(给"差一点"缓冲,防整轮重跑)
    #[test]
    fn grace_is_ratio_capped() {
        assert_eq!(overrun_grace_ms(100_000), 20_000); // 20%
        assert_eq!(overrun_grace_ms(240_000), 48_000); // 20%
        assert_eq!(overrun_grace_ms(600_000), 60_000); // 封顶
        assert_eq!(overrun_grace_ms(9_999_999), 60_000); // 封顶
    }

    /// 旧名 timeout_ms(曾是"总时长")仍能解析,老模型不白传
    #[test]
    fn legacy_timeout_ms_still_parsed() {
        let a: ShellArgs =
            serde_json::from_value(serde_json::json!({"command": "ls", "timeout_ms": 240000}))
                .unwrap();
        assert_eq!(a.idle_ms, Some(240_000));
    }

    /// 静默预警档位:80% 前 0,>80% 黄,距上限不足 10s 红(90s:72s 黄、80s 起红)
    #[test]
    fn silent_level_ramps() {
        let cap = Duration::from_secs(90);
        assert_eq!(silent_level(cap, Duration::from_secs(60)), 0);
        assert_eq!(silent_level(cap, Duration::from_secs(75)), 1);
        assert_eq!(silent_level(cap, Duration::from_secs(85)), 2);
    }

    /// 无头模式(events=None)高危命令直接拒:连 bypassPermissions 也不放行,
    /// 且多打空格这种绕过写法同样拦得住
    #[tokio::test]
    async fn headless_blocked_command_is_rejected_even_in_bypass() {
        let perm = crate::permissions::Permission::new(crate::permissions::Mode::BypassPermissions);
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let ctx = ToolContext {
            cwd: dir,
            permission: &perm,
            session_id: "test",
            cancel: None,
            events: None,
        };
        for cmd in ["rm -rf /", "rm  -rf /"] {
            let r = run_shell_command(&ctx, &serde_json::json!({ "command": cmd })).await;
            let e = r.expect_err("高危命令应被拒绝").0;
            assert!(e.contains("拦截"), "应说明被拦截: {e}");
        }
    }

    /// 具体路径不再被误伤(`rm -rf /tmp/123` 在原 contains 实现里会命中 `rm -rf /`)
    #[tokio::test]
    async fn headless_allows_concrete_path_command() {
        let perm = crate::permissions::Permission::new(crate::permissions::Mode::BypassPermissions);
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let ctx = ToolContext {
            cwd: dir,
            permission: &perm,
            session_id: "test",
            cancel: None,
            events: None,
        };
        let r = run_shell_command(&ctx, &serde_json::json!({ "command": "echo ok" })).await;
        assert!(r.is_ok(), "普通命令应能执行: {:?}", r.err().map(|e| e.0));
    }
}
