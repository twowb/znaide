//! 会话管理窗口(`/resume` 无参打开):全屏覆盖的交互式列表,
//! 管理**历史会话**与**长期记忆**两个页签。
//!
//! 复用 config 向导的"全屏覆盖"模式:窗口打开时渲染优先(画完即 return)、
//! 按键优先(宿主把按键全部交给 `on_key`)。文件侧动作:
//! - 会话删除走 `session::remove_session`(jsonl + 备注 sidecar 一起删);
//! - 会话备注走 `session::read_note`/`write_note`(sidecar,不动 jsonl);
//! - 记忆删除走 `memory::delete_memory`(md + 索引行一起删);
//! - 恢复会话 = 返回 `UiAction::Resume(path)`,由宿主发 `AgentCmd::LoadHistory`。

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use znaide_core::session::{list_history_sessions_detailed, remove_session, write_note};
use znaide_core::tools::memory::{delete_memory, list_memories};

/// 备注长度上限(与 app.rs 的 /note 一致)
const NOTE_MAX_LEN: usize = 200;

/// 页签
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Sessions,
    Memories,
}

impl Tab {
    fn label(self) -> &'static str {
        match self {
            Tab::Sessions => "历史会话",
            Tab::Memories => "长期记忆",
        }
    }
}

/// 会话页一行
#[derive(Debug, Clone)]
struct SessionRow {
    path: PathBuf,
    id: String,
    headless: bool,
    note: Option<String>,
    size_kb: u64,
    modified: u64,
    is_current: bool,
}

/// 记忆页一行
#[derive(Debug, Clone)]
struct MemoryRow {
    filename: String,
    name: String,
    desc: String,
    mtype: String,
    size_kb: u64,
    modified: u64,
}

/// 窗口内部模式
enum Mode {
    Browse,
    /// 删除确认(y 执行 / n、Esc 取消)
    ConfirmDelete,
    /// 编辑会话备注(尾部输入)
    EditNote(String),
    /// 过滤词输入(尾部输入)
    Filter(String),
    /// 查看记忆正文
    Detail { title: String, body: Vec<String>, offset: usize },
}

/// 返回给宿主跨层执行的动作
#[derive(Debug)]
pub enum UiAction {
    None,
    Exit,
    /// 恢复该历史会话(宿主发 AgentCmd::LoadHistory)
    Resume(PathBuf),
}

/// 会话管理窗口
pub struct SessionsUi {
    tab: Tab,
    sessions: Vec<SessionRow>,
    mems: Vec<MemoryRow>,
    sess_marks: Vec<bool>,
    mem_marks: Vec<bool>,
    cursor: usize,
    scroll: usize,
    mode: Mode,
    /// 最近一次操作结果 (是否成功, 文本)
    status: Option<(bool, String)>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// unix 秒 → 粗略日期(UTC 天数算法,免 chrono 依赖;仅作近似展示)
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 相对时间:刚刚 / N 分钟前 / N 小时前 / N 天前 / 日期
fn fmt_ago(secs: u64, now: u64) -> String {
    let diff = now.saturating_sub(secs);
    if diff < 60 {
        "刚刚".to_string()
    } else if diff < 3600 {
        format!("{} 分钟前", diff / 60)
    } else if diff < 86_400 {
        format!("{} 小时前", diff / 3600)
    } else if diff < 30 * 86_400 {
        format!("{} 天前", diff / 86_400)
    } else {
        let (y, m, d) = civil_from_days((secs / 86_400) as i64);
        format!("{y:04}-{m:02}-{d:02}")
    }
}

/// 去掉 frontmatter 段落,返回正文
fn strip_frontmatter(content: &str) -> String {
    let mut body = content;
    if let Some(rest) = body.strip_prefix("---") {
        if let Some(after) = rest.split_once("\n---") {
            body = after.1;
        }
    }
    body.trim_start_matches('\n').to_string()
}

/// 截断展示文本
fn short(s: &str, max: usize) -> String {
    let mut t: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        t.push('…');
    }
    t
}

impl SessionsUi {
    /// 打开窗口:载入全部历史会话与长期记忆(宿主在空闲时调用)
    pub fn open(current_session_id: &str) -> Self {
        let mut sessions = Vec::new();
        for h in list_history_sessions_detailed() {
            let id = h
                .path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let meta = std::fs::metadata(&h.path).ok();
            let is_current = h.path.file_stem().map(|s| s == current_session_id).unwrap_or(false);
            sessions.push(SessionRow {
                path: h.path,
                id,
                headless: h.headless,
                note: h.note,
                size_kb: meta.as_ref().map(|m| m.len() / 1024).unwrap_or(0),
                modified: meta
                    .as_ref()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
                is_current,
            });
        }
        let mems = list_memories()
            .into_iter()
            .map(|m| MemoryRow {
                filename: m.filename,
                name: m.name,
                desc: m.description,
                mtype: m.mtype,
                size_kb: m.size / 1024,
                modified: m.modified,
            })
            .collect::<Vec<_>>();
        let n = sessions.len();
        let m = mems.len();
        SessionsUi {
            tab: Tab::Sessions,
            sessions,
            mems,
            sess_marks: vec![false; n],
            mem_marks: vec![false; m],
            cursor: 0,
            scroll: 0,
            mode: Mode::Browse,
            status: None,
        }
    }

    fn rows_len(&self) -> usize {
        match self.tab {
            Tab::Sessions => self.sessions.len(),
            Tab::Memories => self.mems.len(),
        }
    }

    fn marks(&self) -> &[bool] {
        match self.tab {
            Tab::Sessions => &self.sess_marks,
            Tab::Memories => &self.mem_marks,
        }
    }

    fn marks_mut(&mut self) -> &mut Vec<bool> {
        match self.tab {
            Tab::Sessions => &mut self.sess_marks,
            Tab::Memories => &mut self.mem_marks,
        }
    }

    fn set_status(&mut self, ok: bool, text: impl Into<String>) {
        self.status = Some((ok, text.into()));
    }

    fn clear_status(&mut self) {
        self.status = None;
    }

    fn clamp_cursor(&mut self) {
        let n = self.rows_len();
        if n == 0 {
            self.cursor = 0;
            self.scroll = 0;
            return;
        }
        self.cursor = self.cursor.min(n - 1);
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        let n = self.rows_len();
        if n == 0 {
            return;
        }
        self.cursor = (self.cursor as isize + delta).clamp(0, n as isize - 1) as usize;
        self.clamp_cursor();
    }

    fn cursor_is_current(&self) -> bool {
        matches!(self.tab, Tab::Sessions)
            && self.sessions.get(self.cursor).map(|s| s.is_current).unwrap_or(false)
    }

    fn row_is_current(&self, idx: usize) -> bool {
        matches!(self.tab, Tab::Sessions)
            && self.sessions.get(idx).map(|s| s.is_current).unwrap_or(false)
    }

    fn toggle_mark(&mut self) {
        if self.cursor_is_current() {
            self.set_status(false, "当前会话不能删除,不用标记。");
            return;
        }
        let c = self.cursor;
        if let Some(m) = self.marks_mut().get_mut(c) {
            *m = !*m;
        }
        self.clear_status();
    }

    /// 删除候选:有标记 → 全部标记行;无 → 光标行(当前会话一律除外)
    fn delete_candidates(&self) -> Vec<usize> {
        let marked: Vec<usize> = self
            .marks()
            .iter()
            .enumerate()
            .filter(|(i, m)| **m && !self.row_is_current(*i))
            .map(|(i, _)| i)
            .collect();
        if !marked.is_empty() {
            return marked;
        }
        if self.rows_len() != 0 && !self.row_is_current(self.cursor) {
            vec![self.cursor]
        } else {
            Vec::new()
        }
    }

    fn commit_delete(&mut self) {
        let idxs = self.delete_candidates();
        if idxs.is_empty() {
            self.set_status(false, "没有可删除的行(当前会话除外)。");
            return;
        }
        let (mut done, mut failed) = (0usize, 0usize);
        match self.tab {
            Tab::Sessions => {
                for &i in &idxs {
                    match remove_session(&self.sessions[i].path) {
                        Ok(()) => done += 1,
                        Err(e) => {
                            failed += 1;
                            self.set_status(false, format!("删除失败: {e}"));
                        }
                    }
                }
            }
            Tab::Memories => {
                for &i in &idxs {
                    match delete_memory(&self.mems[i].filename) {
                        Ok(true) => done += 1,
                        Ok(false) => {
                            failed += 1;
                            self.set_status(false, format!("「{}」不存在", self.mems[i].name));
                        }
                        Err(e) => {
                            failed += 1;
                            self.set_status(false, format!("删除失败: {e}"));
                        }
                    }
                }
            }
        }
        // 倒序移除行与标记
        for &i in idxs.iter().rev() {
            match self.tab {
                Tab::Sessions => {
                    self.sessions.remove(i);
                    self.sess_marks.remove(i);
                }
                Tab::Memories => {
                    self.mems.remove(i);
                    self.mem_marks.remove(i);
                }
            }
        }
        if failed == 0 {
            self.set_status(true, format!("✔ 已删除 {done} 条。"));
        } else {
            self.set_status(false, format!("已删除 {done} 条,{failed} 条失败。"));
        }
        self.clamp_cursor();
    }

    fn commit_note(&mut self, text: String) {
        let Some(row) = self.sessions.get_mut(self.cursor) else {
            return;
        };
        match write_note(&row.path, &text) {
            Ok(()) => {
                let trimmed = text.trim();
                row.note = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                };
                let brief = row
                    .note
                    .as_deref()
                    .map(|n| format!("「{}」", short(n, 40)))
                    .unwrap_or_else(|| "已清除".into());
                self.set_status(true, format!("✔ 备注已更新:{brief}"));
            }
            Err(e) => self.set_status(false, format!("写备注失败: {e}")),
        }
    }

    fn open_detail(&mut self) {
        let Some(row) = self.mems.get(self.cursor) else {
            return;
        };
        let raw = std::fs::read_to_string(
            znaide_core::config::data_dir()
                .join("memories")
                .join(format!("{}.md", row.filename)),
        )
        .unwrap_or_default();
        let body: Vec<String> = strip_frontmatter(&raw).lines().map(|l| l.to_string()).collect();
        self.mode = Mode::Detail {
            title: format!("{} — {}", row.name, row.desc),
            body,
            offset: 0,
        };
    }

    /// 键盘入口。窗口打开时宿主把所有按键交给这里。
    pub fn on_key(&mut self, key: KeyEvent) -> UiAction {
        // 正文查看
        if let Mode::Detail { offset, .. } = &mut self.mode {
            match key.code {
                KeyCode::Esc => self.mode = Mode::Browse,
                KeyCode::Up => *offset = offset.saturating_sub(1),
                KeyCode::Down => *offset += 1,
                KeyCode::PageUp => *offset = offset.saturating_sub(15),
                KeyCode::PageDown => *offset += 15,
                KeyCode::Home => *offset = 0,
                KeyCode::End => *offset = usize::MAX,
                _ => {}
            }
            return UiAction::None;
        }
        // 删除确认
        if let Mode::ConfirmDelete = self.mode {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.mode = Mode::Browse;
                    self.commit_delete();
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.mode = Mode::Browse;
                    self.clear_status();
                }
                _ => {}
            }
            return UiAction::None;
        }
        // 备注编辑
        if let Mode::EditNote(buf) = &mut self.mode {
            match key.code {
                KeyCode::Enter => {
                    let text = buf.clone();
                    self.mode = Mode::Browse;
                    self.commit_note(text);
                }
                KeyCode::Esc => {
                    self.mode = Mode::Browse;
                    self.clear_status();
                }
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(c) if !c.is_control() => {
                    if buf.chars().count() < NOTE_MAX_LEN {
                        buf.push(c);
                    } else {
                        self.set_status(false, format!("备注最长 {NOTE_MAX_LEN} 字。"));
                    }
                }
                _ => {}
            }
            return UiAction::None;
        }
        // 过滤输入
        if let Mode::Filter(buf) = &mut self.mode {
            match key.code {
                KeyCode::Enter | KeyCode::Esc => {
                    self.mode = Mode::Browse;
                    self.clear_status();
                    self.clamp_cursor();
                }
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(c) if !c.is_control() => {
                    buf.push(c);
                }
                _ => {}
            }
            return UiAction::None;
        }

        // Browse
        match key.code {
            KeyCode::Esc => UiAction::Exit,
            KeyCode::Tab => {
                self.tab = match self.tab {
                    Tab::Sessions => Tab::Memories,
                    Tab::Memories => Tab::Sessions,
                };
                self.cursor = 0;
                self.scroll = 0;
                self.clear_status();
                UiAction::None
            }
            KeyCode::Up => {
                self.move_cursor(-1);
                UiAction::None
            }
            KeyCode::Down => {
                self.move_cursor(1);
                UiAction::None
            }
            KeyCode::PageUp => {
                self.move_cursor(-10);
                UiAction::None
            }
            KeyCode::PageDown => {
                self.move_cursor(10);
                UiAction::None
            }
            KeyCode::Home => {
                self.move_cursor(-(self.cursor as isize + 1));
                UiAction::None
            }
            KeyCode::End => {
                let n = self.rows_len();
                if n > 0 {
                    self.move_cursor(n as isize);
                }
                UiAction::None
            }
            KeyCode::Enter => {
                if self.empty() {
                    return UiAction::None;
                }
                match self.tab {
                    Tab::Sessions => {
                        if self.cursor_is_current() {
                            self.set_status(false, "这是当前会话,直接回车即可继续对话。");
                            UiAction::None
                        } else {
                            UiAction::Resume(self.sessions[self.cursor].path.clone())
                        }
                    }
                    Tab::Memories => {
                        self.open_detail();
                        UiAction::None
                    }
                }
            }
            KeyCode::Char(' ') | KeyCode::Char('x') | KeyCode::Char('X') => {
                self.toggle_mark();
                UiAction::None
            }
            KeyCode::Char('d') | KeyCode::Char('D') | KeyCode::Delete => {
                if self.delete_candidates().is_empty() {
                    self.set_status(false, "当前会话不能删除(想重新开始用 /clear)。");
                } else {
                    self.mode = Mode::ConfirmDelete;
                }
                UiAction::None
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                if matches!(self.tab, Tab::Sessions) && !self.sessions.is_empty() {
                    let cur = self.sessions[self.cursor].note.clone().unwrap_or_default();
                    self.mode = Mode::EditNote(cur);
                }
                UiAction::None
            }
            KeyCode::Char('/') => {
                self.mode = Mode::Filter(String::new());
                UiAction::None
            }
            _ => UiAction::None,
        }
    }

    fn empty(&self) -> bool {
        self.rows_len() == 0
    }

    /// 渲染(窗口打开时宿主替代消息区调用)
    pub fn render(&self, f: &mut Frame<'_>, area: Rect) {
        let border_style = Style::default().fg(Color::Cyan);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(border_style)
            .title(format!(" 会话管理(/resume) · {}页 ", self.tab.label()));
        let inner = block.inner(area);
        f.render_widget(block, area);
        if inner.height < 5 || inner.width < 24 {
            return;
        }

        // 记忆正文查看:整块展示
        if let Mode::Detail { title, body, offset } = &self.mode {
            let dblock = Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title(format!(" 记忆正文: {}  ", short(title, 60)))
                .title_style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));
            let dinner = dblock.inner(inner);
            f.render_widget(dblock, inner);
            let max_scroll = body.len().saturating_sub(1);
            let txt = if body.is_empty() {
                "(空)".to_string()
            } else {
                body.join("\n")
            };
            f.render_widget(
                Paragraph::new(txt)
                    .style(Style::default().fg(Color::White))
                    .scroll(((*offset).min(max_scroll) as u16, 0)),
                dinner,
            );
            return;
        }

        // 三行布局:页签行 / 上下文行 / 列表
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .split(inner);

        // 页签行:两个页签 chip + 计数 + 快捷键提示
        let mut spans: Vec<Span> = Vec::new();
        for (t, label) in [(Tab::Sessions, "会话"), (Tab::Memories, "记忆")] {
            let active = self.tab == t;
            spans.push(Span::styled(
                format!(" {label} "),
                if active {
                    Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ));
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            format!("共 {} 条 · 已选 {}    ", self.rows_len(), self.marks().iter().filter(|m| **m).count()),
            Style::default().fg(Color::DarkGray),
        ));
        spans.push(Span::styled(
            match self.tab {
                Tab::Sessions => "Enter 恢复 · n 备注 · ",
                Tab::Memories => "Enter 查看正文 · ",
            },
            Style::default().fg(Color::Yellow),
        ));
        spans.push(Span::styled(
            "空格 多选 · d 删除 · / 过滤 · ↑↓ 移动 · Tab 切换 · Esc 关闭",
            Style::default().fg(Color::DarkGray),
        ));
        f.render_widget(Paragraph::new(Line::from(spans)), chunks[0]);

        // 上下文行:按模式显示输入/确认/结果提示
        let (ctx_style, ctx_txt) = match &self.mode {
            Mode::Browse => match &self.status {
                Some((ok, t)) if !t.is_empty() => (
                    Style::default().fg(if *ok { Color::Green } else { Color::Red }),
                    t.clone(),
                ),
                _ => (Style::default().fg(Color::DarkGray), String::new()),
            },
            Mode::ConfirmDelete => (
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                format!("⚠ 删除确认:将删 {} 条(y 确认 / n 或 Esc 取消)", self.marked_count_for_delete()),
            ),
            Mode::EditNote(buf) => (
                Style::default().fg(Color::Cyan),
                format!("✎ 备注(Enter 保存 / Esc 取消,≤{NOTE_MAX_LEN} 字): {buf}"),
            ),
            Mode::Filter(buf) => (
                Style::default().fg(Color::Cyan),
                format!("🔍 过滤(输入即时筛选,Enter/Esc 结束): /{buf}"),
            ),
            Mode::Detail { .. } => (Style::default(), String::new()),
        };
        f.render_widget(Paragraph::new(ctx_txt).style(ctx_style), chunks[1]);

        // 列表行
        let filter = match &self.mode {
            Mode::Filter(buf) => buf.to_lowercase(),
            _ => String::new(),
        };
        let now = now_secs();
        let mut lines: Vec<String> = Vec::new();
        let mut cursor_line: Option<usize> = None; // 过滤后光标行的显示行号
        match self.tab {
            Tab::Sessions => {
                for (i, s) in self.sessions.iter().enumerate() {
                    let hay = format!(
                        "{} {} {}",
                        s.note.clone().unwrap_or_default(),
                        s.id,
                        s.headless
                    )
                    .to_lowercase();
                    if !filter.is_empty() && !hay.contains(&filter) {
                        continue;
                    }
                    let mark = if self.sess_marks[i] { "[×]" } else { "[ ]" };
                    let cur = if i == self.cursor { "▶" } else { " " };
                    let note = s
                        .note
                        .as_deref()
                        .map(|n| format!("「{}」", short(n, 24)))
                        .unwrap_or_default();
                    let flag = if s.headless { " [无头]" } else { "" };
                    let cur_flag = if s.is_current { " [当前会话]" } else { "" };
                    if i == self.cursor {
                        cursor_line = Some(lines.len());
                    }
                    lines.push(format!(
                        "{mark}{cur}{} {}{}{} · {}KB · {}",
                        note,
                        s.id,
                        flag,
                        cur_flag,
                        s.size_kb,
                        fmt_ago(s.modified, now)
                    ));
                }
            }
            Tab::Memories => {
                for (i, m) in self.mems.iter().enumerate() {
                    let hay = format!("{} {} {} {}", m.name, m.desc, m.filename, m.mtype).to_lowercase();
                    if !filter.is_empty() && !hay.contains(&filter) {
                        continue;
                    }
                    let mark = if self.mem_marks[i] { "[×]" } else { "[ ]" };
                    let cur = if i == self.cursor { "▶" } else { " " };
                    let ty = if m.mtype.is_empty() { String::new() } else { format!("({})", m.mtype) };
                    if i == self.cursor {
                        cursor_line = Some(lines.len());
                    }
                    lines.push(format!(
                        "{mark}{cur} {} — {}{ty} · {}KB · {}",
                        m.name,
                        short(&m.desc, 36),
                        m.size_kb,
                        fmt_ago(m.modified, now)
                    ));
                }
            }
        }
        if lines.is_empty() {
            lines.push(if self.sessions.is_empty() && matches!(self.tab, Tab::Sessions) {
                "(暂无历史会话 — 聊几句后会自动产生)".to_string()
            } else {
                "(无匹配/暂无记忆)".to_string()
            });
        }
        // 滚动:让光标行保持在可视区内(光标移动即滚动)
        let view_h = chunks[2].height as usize;
        let target = cursor_line.unwrap_or(self.scroll);
        let scroll = self.scroll_for(target, view_h);
        let txt = lines.join("\n");
        f.render_widget(
            Paragraph::new(txt)
                .style(Style::default().fg(Color::White))
                .scroll((scroll as u16, 0)),
            chunks[2],
        );
    }

    /// 确认框里显示的实际将删条数
    fn marked_count_for_delete(&self) -> usize {
        self.delete_candidates().len()
    }

    /// 让光标行可见的最小滚动值
    fn scroll_for(&self, cursor_line: usize, view_h: usize) -> usize {
        if view_h == 0 {
            return 0;
        }
        let want = self.scroll;
        if cursor_line >= want && cursor_line < want + view_h {
            want
        } else if cursor_line < want {
            cursor_line
        } else {
            cursor_line.saturating_sub(view_h - 1)
        }
    }
}

// ---------- 测试 ----------

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use std::path::Path;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn sess(id: &str, dir: &Path, is_current: bool, note: Option<&str>) -> SessionRow {
        SessionRow {
            path: dir.join(format!("{id}.jsonl")),
            id: id.to_string(),
            headless: id == "bbb",
            note: note.map(|s| s.to_string()),
            size_kb: 1,
            modified: now_secs(),
            is_current,
        }
    }

    fn mem(filename: &str, name: &str) -> MemoryRow {
        MemoryRow {
            filename: filename.to_string(),
            name: name.to_string(),
            desc: format!("关于{name}的记忆"),
            mtype: "project".to_string(),
            size_kb: 1,
            modified: now_secs(),
        }
    }

    /// 构造窗口(文件只放在临时目录,不碰真实 ~/.znaide)
    fn fake_ui() -> (SessionsUi, PathBuf) {
        let dir = std::env::temp_dir().join(format!("znaide_su_ut_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ui = SessionsUi {
            tab: Tab::Sessions,
            sessions: vec![
                sess("aaa", &dir, false, None),
                sess("bbb", &dir, false, Some("改 README")),
                sess("ccc", &dir, true, None), // 当前会话
            ],
            mems: vec![mem("env", "机器环境"), mem("tool", "工具偏好")],
            sess_marks: vec![false; 3],
            mem_marks: vec![false; 2],
            cursor: 0,
            scroll: 0,
            mode: Mode::Browse,
            status: None,
        };
        (ui, dir)
    }

    #[test]
    fn fmt_ago_basics() {
        let now = 2_000_000_000u64; // 2033 年附近,远离下溢
        assert_eq!(fmt_ago(now, now), "刚刚");
        assert!(fmt_ago(now - 500, now).contains("8 分钟前"));
        assert!(fmt_ago(now - 7200, now).contains("2 小时前"));
        assert!(fmt_ago(now - 3 * 86_400, now).contains("3 天前"));
        let date = fmt_ago(now - 400 * 86_400, now);
        assert_eq!(date.chars().count(), 10, "超过 30 天显示日期,got {date}");
        assert!(date.contains('-'));
    }

    #[test]
    fn cursor_moves_and_clamps() {
        let (mut ui, _d) = fake_ui();
        ui.on_key(key(KeyCode::Down));
        assert_eq!(ui.cursor, 1);
        ui.on_key(key(KeyCode::End));
        assert_eq!(ui.cursor, 2);
        ui.on_key(key(KeyCode::Down)); // 到顶不越界
        assert_eq!(ui.cursor, 2);
        ui.on_key(key(KeyCode::Home));
        assert_eq!(ui.cursor, 0);
    }

    #[test]
    fn mark_skips_current_session() {
        let (mut ui, _d) = fake_ui();
        ui.on_key(key(KeyCode::End)); // 当前会话行
        ui.on_key(key(KeyCode::Char(' ')));
        assert!(!ui.sess_marks[2], "当前会话不应被标记");
        ui.on_key(key(KeyCode::Home));
        ui.on_key(key(KeyCode::Char(' ')));
        assert!(ui.sess_marks[0], "普通会话应可标记");
    }

    #[test]
    fn delete_current_blocked_but_others_work() {
        let (mut ui, dir) = fake_ui();
        // 当前会话:按 d → 提示,不进入确认
        ui.on_key(key(KeyCode::End));
        ui.on_key(key(KeyCode::Char('d')));
        assert!(matches!(ui.mode, Mode::Browse));
        assert!(ui.status.as_ref().map(|(_, t)| t.contains("当前会话")).unwrap_or(false));

        // 普通会话:多选两条 → d → y 删除
        ui.on_key(key(KeyCode::Home));
        ui.on_key(key(KeyCode::Char(' ')));
        ui.on_key(key(KeyCode::Down));
        ui.on_key(key(KeyCode::Char(' ')));
        ui.on_key(key(KeyCode::Char('d')));
        assert!(matches!(ui.mode, Mode::ConfirmDelete));
        assert_eq!(ui.marked_count_for_delete(), 2);
        ui.on_key(key(KeyCode::Char('y')));
        assert_eq!(ui.sessions.len(), 1, "两条应被删,剩当前会话");
        assert_eq!(ui.sessions[0].id, "ccc", "当前会话行应保留");
        assert!(!dir.join("aaa.jsonl").exists());
        assert!(!dir.join("bbb.jsonl").exists());
    }

    #[test]
    fn tab_switches_between_pages() {
        let (mut ui, _d) = fake_ui();
        ui.on_key(key(KeyCode::Tab));
        assert_eq!(ui.tab, Tab::Memories);
        ui.on_key(key(KeyCode::Tab));
        assert_eq!(ui.tab, Tab::Sessions);
    }

    #[test]
    fn resume_action_on_enter() {
        let (mut ui, dir) = fake_ui();
        let r = ui.on_key(key(KeyCode::Enter));
        match r {
            UiAction::Resume(p) => assert_eq!(p, dir.join("aaa.jsonl")),
            other => panic!("应 Resume,got {other:?}"),
        }
    }

    #[test]
    fn note_edit_flow() {
        let (mut ui, dir) = fake_ui();
        ui.on_key(key(KeyCode::Char('n')));
        assert!(matches!(ui.mode, Mode::EditNote(_)));
        // Esc 取消
        ui.on_key(key(KeyCode::Esc));
        assert!(matches!(ui.mode, Mode::Browse));
        assert!(ui.sessions[0].note.is_none());

        // 输入 + Enter 保存
        ui.on_key(key(KeyCode::Char('n')));
        for c in "给 README 做英文版".chars() {
            ui.on_key(key(KeyCode::Char(c)));
        }
        ui.on_key(key(KeyCode::Enter));
        assert_eq!(ui.sessions[0].note.as_deref(), Some("给 README 做英文版"));
        assert!(
            dir.join("aaa.meta.json").exists(),
            "备注 sidecar 应写入(临时目录)"
        );
        std::fs::remove_file(dir.join("aaa.meta.json")).ok();
    }

    #[test]
    fn memory_page_enter_opens_detail() {
        let (mut ui, _d) = fake_ui();
        ui.on_key(key(KeyCode::Tab));
        let r = ui.on_key(key(KeyCode::Enter));
        assert!(matches!(r, UiAction::None));
        assert!(matches!(ui.mode, Mode::Detail { .. }));
        ui.on_key(key(KeyCode::Esc));
        assert!(matches!(ui.mode, Mode::Browse));
    }

    #[test]
    fn delete_memory_page_confirmation() {
        let (mut ui, _d) = fake_ui();
        ui.on_key(key(KeyCode::Tab));
        ui.on_key(key(KeyCode::Char(' ')));
        ui.on_key(key(KeyCode::Char('d')));
        assert!(matches!(ui.mode, Mode::ConfirmDelete));
        assert_eq!(ui.marked_count_for_delete(), 1);
        // 记忆文件不在临时目录 → delete_memory 返回 false(找不到),不崩
        ui.on_key(key(KeyCode::Char('y')));
        assert_eq!(ui.mems.len(), 1, "行应移除");
        assert_eq!(ui.mem_marks.len(), 1);
    }
}
