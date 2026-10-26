// znaide GUI 骨架前端：单一 WebSocket，事件驱动。
// 模型与工具输出是不可信输入：markdown 走 marked → DOMPurify，工具输出走 ansi_up → DOMPurify；
// 页面自身不拼接未净化 HTML。协议见 docs/plan-gui.md §8.2。

const $ = (id) => document.getElementById(id);
const stream = $('stream');
const emptyHint = $('empty');
const input = $('input');
const btnSend = $('btn-send');
const btnStop = $('btn-stop');
const connEl = $('conn');
const queueBar = $('queue');
const queueItems = $('queue-items');
const modal = $('modal');
const modalBody = $('modal-body');
const modalInput = $('modal-input');
const modalList = $('modal-list');
const sidebar = $('sidebar');
const sessList = $('sess-list');
const sessSearch = $('sess-search');
const palette = $('palette');
const settingsEl = $('settings');
const setBody = $('set-body');
const chipUpdate = $('chip-update');

const state = {
  ws: null,
  busy: false,
  current: null,        // { node, text }
  reason: null,         // 当前回合的思考块 { node, body }
  tools: new Map(),     // name -> [{ card, pre, buf, started, timer }]
  queue: [],            // 仅用于展示；真正的队列在宿主 inbox
  modal: null,          // { kind, frame }
  tokens: { input: 0, output: 0 },
  ctx: { window: null, used: null }, // used = 最近一次请求的输入 tokens(null = 还没测到)
  status: { model: '', provider: '', mode: 'ask', mode_label: '询问', persona: '', ctx_window: null },
  providers: [],
  personas: [],
  skills: [],
  sessions: [],
  help: '',
  settings: null,
  settingsOpen: false,
  settingsSection: 'model',
  modelInput: null,
  skillEditor: null,
  memoryEditor: null,
  updateNotice: '',
  updateResult: '',
  session: '',
  cwd: '',
  at: null,
  follow: true, // 是否跟随到底部；用户向上滚后暂停，回到底部恢复
  closing: false,
  reconnectMs: 500,
};

/* ---------------- 主题 ---------------- */

const savedTheme = localStorage.getItem('znaide-theme');
if (savedTheme === 'light' || savedTheme === 'dark') {
  document.documentElement.dataset.theme = savedTheme;
}
$('btn-theme').addEventListener('click', () => {
  const next = document.documentElement.dataset.theme === 'light' ? 'dark' : 'light';
  document.documentElement.dataset.theme = next;
  localStorage.setItem('znaide-theme', next);
  applyHljsTheme();
});

/** 代码高亮主题跟随页面主题（两份 hljs CSS 只启用一份） */
function applyHljsTheme() {
  const dark = document.documentElement.dataset.theme !== 'light';
  const darkSheet = $('hljs-dark');
  const lightSheet = $('hljs-light');
  if (darkSheet) darkSheet.disabled = !dark;
  if (lightSheet) lightSheet.disabled = dark;
}

/* ---------------- 访问令牌 ---------------- */

function readCookie(name) {
  for (const part of document.cookie.split(';')) {
    const [k, ...rest] = part.trim().split('=');
    if (k === name) return rest.join('=');
  }
  return '';
}

const token = new URLSearchParams(location.search).get('token') || readCookie('znaide_token') || '';

/* ---------------- 小工具 ---------------- */

function el(tag, cls, text) {
  const node = document.createElement(tag);
  if (cls) node.className = cls;
  if (text != null) node.textContent = text;
  return node;
}

/* ---------------- markdown / 高亮 / ANSI ---------------- */

// 模型输出里的 style/img/iframe/form 一律不要:远程图片是追踪面,样式可用来盖住界面
const MD_SANITIZE = {
  FORBID_TAGS: ['style', 'img', 'iframe', 'object', 'embed', 'form', 'input', 'textarea', 'select', 'button'],
  FORBID_ATTR: ['style', 'srcset', 'formaction'],
};

if (window.DOMPurify) {
  // 外链一律新窗口 + noopener;不把本页交出去
  window.DOMPurify.addHook('afterSanitizeAttributes', (node) => {
    if (node.tagName === 'A') {
      node.setAttribute('target', '_blank');
      node.setAttribute('rel', 'noopener noreferrer');
    }
  });
}

let ansi = null;
import('./vendor/ansi_up.js')
  .then((m) => {
    ansi = new m.AnsiUp();
  })
  .catch(() => {
    // 缺库时工具输出退化为纯文本
  });

function mdToHtml(text) {
  if (!window.marked || !window.DOMPurify) return null;
  try {
    return window.DOMPurify.sanitize(
      window.marked.parse(text, { gfm: true, breaks: true }),
      MD_SANITIZE,
    );
  } catch {
    return null;
  }
}

function highlightCode(scope) {
  if (!window.hljs) return;
  for (const code of scope.querySelectorAll('pre code')) {
    if (code.dataset.highlighted) continue;
    try {
      window.hljs.highlightElement(code);
    } catch {
      // 语言不认识就保持原文
    }
  }
}

function selectionInside(node) {
  const sel = window.getSelection();
  return Boolean(
    sel && !sel.isCollapsed && (node.contains(sel.anchorNode) || node.contains(sel.focusNode)),
  );
}

/// 流式期间节流重绘;完成时 final=true(补高亮,且不跳过选区检查)
function paintAssistant(block, final) {
  if (!block) return;
  if (!final && selectionInside(block.node)) return; // 用户正在选字,别把选区打掉
  const html = mdToHtml(block.text);
  if (html == null) {
    block.node.textContent = block.text;
    return;
  }
  block.node.innerHTML = html;
  if (final) highlightCode(block.node);
}

let paintTimer = null;

function scheduleAssistantPaint() {
  if (paintTimer) return;
  paintTimer = setTimeout(() => {
    paintTimer = null;
    paintAssistant(state.current, false);
  }, 120);
}

function flushAssistantPaint(block) {
  if (paintTimer) {
    clearTimeout(paintTimer);
    paintTimer = null;
  }
  paintAssistant(block, true);
}

/// 工具输出:ANSI SGR → HTML(库自己转义文本),再净化一次
function paintToolOutput(entry) {
  if (!window.DOMPurify || !ansi) {
    entry.pre.textContent = entry.buf;
    return;
  }
  try {
    entry.pre.innerHTML = window.DOMPurify.sanitize(ansi.ansi_to_html(entry.buf));
  } catch {
    entry.pre.textContent = entry.buf;
  }
}

function nearBottom() {
  return stream.scrollHeight - stream.scrollTop - stream.clientHeight < 80;
}

function toBottom(force = false) {
  if (force) state.follow = true;
  if (!state.follow) return;
  stream.scrollTop = stream.scrollHeight;
}

function hideEmpty() {
  emptyHint.hidden = true;
}

function clearStream() {
  for (const node of [...stream.children]) {
    if (node !== emptyHint) node.remove();
  }
  emptyHint.hidden = true;
  state.current = null;
  state.tools.clear();
}

function notice(text, level = '') {
  hideEmpty();
  const node = el('div', `notice ${level}`.trim(), text);
  stream.append(node);
  toBottom();
  return node;
}

function renderUser(text, injected = false) {
  hideEmpty();
  const wrap = el('article', `msg user${injected ? ' injected' : ''}`);
  wrap.append(el('div', 'who', injected ? '你（插话）' : '你'));
  wrap.append(el('div', 'body', text));
  stream.append(wrap);
  toBottom();
}

function renderAssistantText(text) {
  hideEmpty();
  const wrap = el('article', 'msg assistant');
  wrap.append(el('div', 'who', 'znaide'));
  const body = el('div', 'body md');
  wrap.append(body);
  stream.append(wrap);
  paintAssistant({ node: body, text }, true);
  return body;
}

function ensureAssistant() {
  if (state.current) return state.current;
  hideEmpty();
  const wrap = el('article', 'msg assistant');
  wrap.append(el('div', 'who', 'znaide'));
  const body = el('div', 'body md streaming');
  wrap.append(body);
  stream.append(wrap);
  state.current = { node: body, text: '' };
  toBottom(true);
  return state.current;
}

function setBusy(busy) {
  state.busy = busy;
  btnStop.hidden = !busy;
  btnSend.textContent = busy ? '排队' : '发送';
  if (busy) hidePalette();
}

/* ---------------- 工具卡片 ---------------- */

function oneLine(s, max = 120) {
  const flat = String(s || '').replace(/\s+/g, ' ').trim();
  return flat.length > max ? flat.slice(0, max) + '…' : flat;
}

function toolCard(name, args, idleMs, live = true) {
  const card = el('details', 'tool running');
  card.open = true;

  const summary = el('summary');
  const marker = el('span', 'tool-state', '▶');
  const nameEl = el('span', 'tool-name', name);
  const argsEl = el('span', 'tool-args', oneLine(args) || '（无参数）');
  argsEl.title = args || '';
  const timeEl = el('span', 'tool-time', live ? '0.0s' : '历史');
  if (idleMs) timeEl.title = `静默预算 ${idleMs} ms`;
  summary.append(marker, nameEl, argsEl, timeEl);

  const pre = el('pre');
  card.append(summary, pre);
  hideEmpty();
  stream.append(card);
  toBottom(true);

  const started = Date.now();
  const timer = live
    ? setInterval(() => {
        timeEl.textContent = `${((Date.now() - started) / 1000).toFixed(1)}s`;
      }, 200)
    : null;

  const entry = { card, pre, buf: '', started, timer };
  const list = state.tools.get(name) || [];
  list.push(entry);
  state.tools.set(name, list);
  return entry;
}

function runningTool(name) {
  const list = state.tools.get(name) || [];
  for (let i = list.length - 1; i >= 0; i -= 1) {
    if (list[i].card.classList.contains('running')) return list[i];
  }
  return null;
}

function markToolDone(entry, ok, secs) {
  if (entry.timer) clearInterval(entry.timer);
  entry.timer = null;
  entry.card.classList.remove('running', 'warn', 'danger');
  entry.card.classList.add(ok ? 'ok' : 'failed');
  entry.card.querySelector('.tool-state').textContent = ok ? '✓' : '✗';
  entry.card.querySelector('.tool-time').textContent =
    secs == null ? '历史' : `${secs.toFixed(1)}s`;
}

/// 历史会话:assistant 声明的调用先建卡,tool 结果按 tool_call_id 回填(与 TUI 同口径)
function renderMessages(messages) {
  const cards = new Map();
  for (const m of messages) {
    if (m.role === 'user') {
      if (m.text) renderUser(m.text);
    } else if (m.role === 'assistant') {
      if (m.text) renderAssistantText(m.text);
      for (const call of m.tool_calls || []) {
        cards.set(call.id, toolCard(call.name, call.args, null, false));
      }
    } else if (m.role === 'tool') {
      const entry = cards.get(m.tool_call_id);
      if (entry) {
        entry.buf = m.text || '';
        paintToolOutput(entry);
        markToolDone(entry, true);
        entry.card.open = false;
      } else if (m.text) {
        notice(`工具结果：${oneLine(m.text, 300)}`);
      }
    }
  }
}

/* ---------------- 询问弹窗 ---------------- */

function modalButton(label, cls, onClick, opts = {}) {
  const b = el('button', `btn ${cls}`.trim(), label);
  b.disabled = Boolean(opts.disabled);
  if (opts.title) b.title = opts.title;
  b.addEventListener('click', onClick);
  return b;
}

function openModal(kind, frame = {}) {
  state.modal = { kind, frame };
  const title = $('modal-title');
  const body = modalBody;
  const warning = $('modal-warning');
  const hint = $('modal-hint');
  const actions = $('modal-actions');
  actions.textContent = '';
  body.hidden = false;
  body.textContent = '';
  modalInput.hidden = true;
  modalInput.value = '';
  modalList.hidden = true;
  modalList.textContent = '';
  hint.hidden = true;
  warning.hidden = true;

  if (kind === 'permission') {
    title.textContent = frame.title || '需要确认';
    body.textContent = frame.body || '';
    if (frame.warning) {
      warning.hidden = false;
      warning.textContent = frame.warning;
    }
    actions.append(
      modalButton('拒绝', 'ghost', () => answerPermission(false, false)),
      modalButton('允许一次', 'primary', () => answerPermission(true, false)),
      modalButton(
        '本会话总是允许',
        '',
        () => answerPermission(true, true),
        {
          disabled: !frame.allow_always,
          title: frame.allow_always ? '' : '高危操作不提供此选项（只允许单次放行）',
        },
      ),
    );
    if (!frame.allow_always) {
      hint.hidden = false;
      hint.textContent = '高危操作只允许单次放行；即使前端发错，宿主也会拒绝“总是允许”。';
    }
  } else if (kind === 'clear') {
    title.textContent = frame.title || '清空会话?';
    body.textContent = frame.body || '';
    actions.append(
      modalButton('取消', 'ghost', closeModal),
      modalButton('清空', 'primary', () => {
        send({ type: 'clear' });
        closeModal();
      }),
    );
  } else if (kind === 'mode') {
    title.textContent = frame.title || '切换权限档?';
    body.textContent = frame.body || '';
    actions.append(
      modalButton('取消', 'ghost', closeModal),
      modalButton('切换', 'primary', () => {
        send({ type: 'mode', mode: frame.mode });
        closeModal();
      }),
    );
    hint.hidden = false;
    hint.textContent = '切换后立即生效；重启不会保留（启动参数/配置仍是默认来源）。';
  } else if (kind === 'delete') {
    title.textContent = '删除这个会话?';
    body.textContent = `${frame.label || ''}\n${frame.id || ''}\n\n历史文件会从磁盘删除，不可恢复。`;
    actions.append(
      modalButton('取消', 'ghost', closeModal),
      modalButton('删除', 'danger', () => {
        send({ type: 'delete_session', id: frame.id });
        closeModal();
      }),
    );
  } else if (kind === 'rename') {
    title.textContent = '会话备注（显示为侧栏标题）';
    body.hidden = true;
    modalInput.hidden = false;
    modalInput.value = frame.note || '';
    modalInput.placeholder = '留空 = 清除备注';
    setTimeout(() => modalInput.focus(), 0);
    actions.append(
      modalButton('取消', 'ghost', closeModal),
      modalButton('保存', 'primary', () => {
        send({ type: 'rename_session', id: frame.id, note: modalInput.value.trim() });
        closeModal();
      }),
    );
  } else if (kind === 'undo') {
    title.textContent = '回滚文件（点选即恢复）';
    body.hidden = true;
    const items = frame.items || [];
    if (!items.length) {
      body.hidden = false;
      body.textContent = '暂无 undo 快照（还没有文件被修改过）。';
    } else {
      modalList.hidden = false;
      for (const it of items) {
        const row = el('button', 'undo-row');
        const main = el('div', 'undo-main');
        main.append(el('div', 'undo-path', it.orig));
        const bits = [`#${it.index}`, it.action, fmtAgo(it.ts_ms)];
        if (it.external) bits.push('外部');
        main.append(el('div', 'undo-meta', bits.join(' · ')));
        row.append(main, el('span', 'undo-go', '回滚'));
        row.addEventListener('click', () => {
          send({ type: 'undo', index: it.index });
          closeModal();
        });
        modalList.append(row);
      }
    }
    actions.append(modalButton('关闭', 'ghost', closeModal));
  } else if (kind === 'skargs') {
    title.textContent = `运行技能「${frame.name}」`;
    body.hidden = true;
    modalInput.hidden = false;
    modalInput.value = '';
    modalInput.placeholder = '参数（可留空）';
    setTimeout(() => modalInput.focus(), 0);
    actions.append(
      modalButton('取消', 'ghost', closeModal),
      modalButton('运行', 'primary', () => {
        send({ type: 'skill_run', name: frame.name, args: modalInput.value });
        closeModal();
        closeSettings();
      }),
    );
  } else if (kind === 'memdel') {
    title.textContent = '删除这条记忆?';
    body.textContent = `${frame.name}\n${frame.scope === 'global' ? 'Global' : 'Project'} · ${frame.filename}`;
    actions.append(
      modalButton('取消', 'ghost', closeModal),
      modalButton('删除', 'danger', () => {
        send({ type: 'memory_delete', filename: frame.filename, scope: frame.scope });
        closeModal();
      }),
    );
  } else if (kind === 'help') {
    title.textContent = '命令与操作';
    body.textContent = state.help || '（帮助文本未随快照下发）';
    actions.append(modalButton('关闭', 'primary', closeModal));
  } else {
    title.textContent = '轮次预算用尽';
    body.textContent = `已用 ${frame.used} 轮，任务可能还没完。`;
    actions.append(
      modalButton('就此收尾', 'ghost', () => answerRounds(false)),
      modalButton('继续执行', 'primary', () => answerRounds(true)),
    );
    hint.hidden = false;
    hint.textContent = '继续会再放一批（默认 200 轮）。';
  }

  modal.hidden = false;
}

function openHelpModal() {
  openModal('help');
}

function openUndoModal(items) {
  openModal('undo', { items });
}

function openDeleteModal(s) {
  openModal('delete', { id: s.id, label: s.note || s.title || s.id });
}

function openRenameModal(s) {
  openModal('rename', { id: s.id, note: s.note || '' });
}

modalInput.addEventListener('keydown', (e) => {
  if (e.key !== 'Enter') return;
  e.preventDefault();
  const primary = $('modal-actions').querySelector('.btn.primary');
  if (primary) primary.click();
});

function closeModal() {
  modal.hidden = true;
  state.modal = null;
}

function answerPermission(allow, always) {
  send({ type: 'permission', allow, always });
  closeModal();
}

function answerRounds(cont) {
  send({ type: 'rounds', cont });
  closeModal();
}

function showAsk(frame) {
  if (state.modal) return; // 已有弹窗时忽略重复帧（例如重连补发）
  openModal(frame.kind, frame);
}

/* ---------------- 队列展示 ---------------- */

function syncQueue() {
  queueBar.hidden = state.queue.length === 0;
  queueItems.textContent = '';
  for (const text of state.queue) {
    const chip = el('span', 'queue-chip', oneLine(text, 60));
    chip.title = text;
    const drop = el('button', 'queue-x', '×');
    drop.title = '撤回这条';
    drop.setAttribute('aria-label', '撤回这条');
    drop.addEventListener('click', () => {
      const idx = state.queue.indexOf(text);
      if (idx < 0) return;
      state.queue.splice(idx, 1);
      syncQueue();
      // 宿主 inbox 是真正的队列:撤回失败(已被取走)会以 error 帧回来
      send({ type: 'unqueue', text });
    });
    chip.append(drop);
    queueItems.append(chip);
  }
}

/* ---------------- slash 命令 ---------------- */

function renderCommandEcho(line) {
  hideEmpty();
  const wrap = el('article', 'msg user');
  wrap.append(el('div', 'who', '你'));
  wrap.append(el('div', 'body cmd', line));
  stream.append(wrap);
  toBottom();
}

function renderCommandOutput(text) {
  hideEmpty();
  const pre = el('pre', 'cmd-output', text);
  stream.append(pre);
  toBottom();
}

/* ---------------- 命令面板(输入 / 弹出) ---------------- */

const COMMANDS = [
  { name: '/config', desc: '打开设置页（模型/网络/人格/MCP…）' },
  { name: '/help', desc: '命令与操作帮助' },
  { name: '/clear', desc: '清空当前会话（需确认）' },
  { name: '/compact', desc: '压缩上下文' },
  { name: '/resume', desc: '会话列表（展开侧栏）' },
  { name: '/undo', desc: 'undo 快照（点选回滚）' },
  { name: '/skills', desc: '列出已安装技能' },
  { name: '/persona', desc: '查看/切换人格' },
  { name: '/update', desc: '检查并安装更新' },
  { name: '/quit', desc: '退出 znaide' },
];

let paletteList = [];
let paletteIndex = -1;

function hidePalette() {
  palette.hidden = true;
  palette.textContent = '';
  paletteList = [];
  paletteIndex = -1;
}

function updatePalette() {
  const v = input.value;
  if (state.closing || state.busy || !v.startsWith('/') || /\s/.test(v)) {
    hidePalette();
    return;
  }
  const q = v.slice(1).toLowerCase();
  const all = [
    ...COMMANDS,
    ...state.skills.map((s) => ({ name: `/${s.name}`, desc: s.desc, skill: true })),
  ];
  paletteList = all
    .filter((i) => {
      const name = i.name.slice(1).toLowerCase();
      return name.startsWith(q) || (q && i.desc.toLowerCase().includes(q));
    })
    .slice(0, 12);
  if (!paletteList.length) {
    hidePalette();
    return;
  }
  paletteIndex = 0;
  paintPalette();
  palette.hidden = false;
}

function paintPalette() {
  palette.textContent = '';
  paletteList.forEach((item, i) => {
    const b = el('button', `palette-item${i === paletteIndex ? ' active' : ''}`);
    b.append(el('span', 'palette-name', item.name));
    b.append(el('span', 'palette-desc', item.desc || ''));
    // 别让 textarea 在点选前失焦
    b.addEventListener('mousedown', (e) => e.preventDefault());
    b.addEventListener('click', () => pickPalette(item));
    palette.append(b);
  });
}

function pickPalette(item) {
  if (item.path !== undefined) {
    const c = state.at;
    if (!c) return;
    const end = input.selectionStart ?? input.value.length;
    const ins = `@${quoteAtPath(item.path)}` + (item.dir ? '' : ' ');
    input.value = input.value.slice(0, c.start) + ins + input.value.slice(end);
    const pos = c.start + ins.length;
    input.selectionStart = input.selectionEnd = pos;
    state.at = null;
    hidePalette();
    autosize();
    input.focus();
    return;
  }
  if (item.skill) {
    input.value = `${item.name} `;
    hidePalette();
    autosize();
    input.focus();
    return;
  }
  input.value = item.name;
  hidePalette();
  submit();
}

function pushQueue(text) {
  state.queue.push(text);
  syncQueue();
}

function popQueue(text) {
  if (!state.queue.length) return null;
  let idx = state.queue.indexOf(text);
  if (idx < 0) idx = 0;
  return state.queue.splice(idx, 1)[0];
}

/* ---------------- 事件 ---------------- */

function onHello(frame) {
  setConn('on', '已连接');

  renderCwd(frame.cwd || '');
  state.providers = frame.providers || [];
  state.personas = frame.personas || [];
  state.skills = frame.skills || [];
  state.help = frame.help || '';

  applyStatus({
    model: frame.model || '',
    provider: frame.provider || '',
    mode: frame.mode || 'ask',
    mode_label: frame.mode_label || '询问',
    persona: frame.persona || '',
    ctx_window: frame.ctx_window ?? null,
  });
  // 顶栏被改过(重连)时以 live 状态为准
  if (frame.status) applyStatus(frame.status);

  if (frame.session_id) setSession(frame.session_id);
  renderSessions(frame.sessions || []);
  if (frame.update_notice) showUpdateChip(frame.update_notice);
  // 首启(没有任何配置来源):直接把设置页当配置向导打开
  if (!state.status.configured) openSettings('model');

  if (frame.resume) notice(`▶ 正在恢复会话 ${frame.resume}…`);

  for (const n of frame.notes || []) {
    notice(n.text, n.level === 'warn' ? 'warn' : '');
  }
  if (frame.resolve_error) {
    notice(`配置未就绪：${frame.resolve_error}\n先在终端运行一次 \`znaide\` 完成配置向导，再回到本页面。`, 'error');
  } else if (frame.first_run) {
    notice('还没有可用的模型配置。先在终端运行 `znaide` 完成引导，或带 --provider 启动本程序。', 'warn');
  }

  if (frame.pending_ask) showAsk(frame.pending_ask);
}

function renderCwd(path) {
  state.cwd = path || '';
  const chip = $('chip-cwd');
  const base = state.cwd.split(/[\\/]/).filter(Boolean).pop() || state.cwd;
  chip.textContent = `📁 ${base || '—'}`;
  chip.title = state.cwd;
}

function quoteAtPath(p) {
  return /[\s,。;)\]}：:]/.test(p) ? `"${p}"` : p;
}

function atContext() {
  const pos = input.selectionStart ?? 0;
  const m = input.value.slice(0, pos).match(/(^|\s)@([^\s]*)$/);
  if (!m) return null;
  return { start: pos - m[2].length - 1, prefix: m[2] };
}

function updateAtPalette() {
  const c = atContext();
  if (!c) {
    if (state.at) {
      state.at = null;
      if (!input.value.startsWith('/')) hidePalette();
    }
    return;
  }
  state.at = c;
  send({ type: 'list_paths', prefix: c.prefix });
}

function setSession(id) {
  state.session = id || '';
  const chip = $('chip-session');
  chip.hidden = !id;
  chip.textContent = id ? `会话 ${id.slice(0, 8)}` : '';
  chip.title = id;
  renderSessions(state.sessions);
}

/* ---------------- ctx 占用 ---------------- */

function fmtTokens(n) {
  return n >= 1000 ? `${(n / 1000).toFixed(1)}k` : String(n);
}

/// 与 TUI 同口径:占用没测到 = `ctx ?`(不拿 0% 冒充);窗口未知 = 只报绝对量、不编百分比。
function renderCtx() {
  const chip = $('chip-ctx');
  const used = state.ctx.used;
  const window = state.ctx.window;
  chip.classList.remove('ctx-warn', 'ctx-danger');

  if (!window) {
    chip.textContent = used ? `ctx ~${fmtTokens(used)} 窗口未知` : 'ctx ? 窗口未知';
    chip.title = '模型不在内置窗口表里,配置也没写死:只报最近一次请求的输入 tokens,不编百分比';
    return;
  }
  if (used == null) {
    chip.textContent = 'ctx ?';
    chip.title = `上下文窗口 ${fmtTokens(window)};占用要等下一轮回复的 usage 才测得到(恢复/压缩后也是未知)`;
    return;
  }
  const pct = Math.min(100, Math.round((used * 100) / window));
  const filled = Math.round(pct / 10);
  chip.textContent = `ctx ${'█'.repeat(filled)}${'░'.repeat(10 - filled)} ${pct}%`;
  chip.title = `上下文占用:${used} / ${window} tokens(${pct}%)· 90% 建议 /compact`;
  if (pct >= 90) chip.classList.add('ctx-danger');
  else if (pct >= 70) chip.classList.add('ctx-warn');
}

/* ---------------- 顶栏状态 / 菜单 ---------------- */

function applyStatus(s) {
  state.status = { ...state.status, ...s };
  const chipMode = $('chip-mode');
  chipMode.textContent = state.status.mode_label || state.status.mode || '询问';
  chipMode.dataset.mode = state.status.mode || 'ask';

  const chipModel = $('chip-model');
  chipModel.textContent = state.status.model || '未配置';
  chipModel.title = state.status.provider
    ? `${state.status.provider} · 点开切换服务商`
    : '点开切换服务商';

  $('chip-persona').textContent = state.status.persona
    ? `人格：${state.status.persona}`
    : '人格：无';

  if ('ctx_window' in s) state.ctx.window = s.ctx_window ?? null;
  renderCtx();
  renderSessions(state.sessions);
}

let openMenuEl = null;

function closeMenu() {
  if (openMenuEl) {
    openMenuEl.remove();
    openMenuEl = null;
  }
}

function showMenu(anchor, items) {
  closeMenu();
  const menu = el('div', 'menu');
  for (const item of items) {
    if (item.sep) {
      menu.append(el('div', 'menu-sep'));
      continue;
    }
    const b = el('button', 'menu-item');
    if (item.active) b.classList.add('active');
    if (item.danger) b.classList.add('danger');
    b.append(el('span', 'menu-label', item.label));
    if (item.hint) b.append(el('span', 'menu-hint', item.hint));
    b.addEventListener('click', () => {
      closeMenu();
      item.onClick();
    });
    menu.append(b);
  }
  document.body.append(menu);
  const r = anchor.getBoundingClientRect();
  const left = Math.min(r.left, window.innerWidth - menu.offsetWidth - 12);
  menu.style.left = `${Math.max(8, left)}px`;
  menu.style.top = `${r.bottom + 6}px`;
  openMenuEl = menu;
}

document.addEventListener('click', (e) => {
  if (openMenuEl && !openMenuEl.contains(e.target)) closeMenu();
});

const MODES = [
  { key: 'ask', label: '询问', hint: '写文件/执行命令前都要确认' },
  { key: 'acceptEdits', label: '编辑放行', hint: '文件自动放行,命令仍确认' },
  { key: 'bypassPermissions', label: '全自动', hint: '全部自动,高危命令仍确认' },
  { key: 'yolo', label: '超级（YOLO）', hint: '一切放行,高危也跳过——危险' },
];

function openModeMenu(e) {
  e.stopPropagation();
  showMenu(
    $('chip-mode'),
    MODES.map((m) => ({
      label: m.label,
      hint: m.hint,
      active: state.status.mode === m.key,
      danger: m.key === 'yolo',
      onClick: () => requestMode(m.key),
    })),
  );
}

function requestMode(key) {
  if (key === state.status.mode) return;
  if (key === 'bypassPermissions' || key === 'yolo') {
    const m = MODES.find((x) => x.key === key);
    openModal('mode', {
      mode: key,
      title: `切换到「${m.label}」?`,
      body: m.hint,
    });
    return;
  }
  send({ type: 'mode', mode: key });
}

function openProviderMenu(e) {
  e.stopPropagation();
  if (!state.providers.length) {
    notice('配置里没有可选的服务商;先运行 `znaide` 的配置向导添加。', 'warn');
    return;
  }
  showMenu(
    $('chip-model'),
    state.providers.map((p) => ({
      label: p.name,
      hint: p.model,
      active: state.status.provider === p.name,
      onClick: () => send({ type: 'provider', name: p.name }),
    })),
  );
}

function openPersonaMenu(e) {
  e.stopPropagation();
  const items = [
    {
      label: '无（默认助手）',
      active: !state.status.persona,
      onClick: () => send({ type: 'persona', name: '' }),
    },
  ];
  for (const name of state.personas) {
    items.push({
      label: name,
      active: state.status.persona === name,
      onClick: () => send({ type: 'persona', name }),
    });
  }
  showMenu($('chip-persona'), items);
}

function openMoreMenu(e) {
  e.stopPropagation();
  showMenu($('btn-more'), [
    {
      label: '设置…',
      hint: '模型 / 网络 / 人格 / 技能 / 记忆 / MCP',
      onClick: () => openSettings(),
    },
    { sep: true },
    {
      label: '回滚文件…',
      hint: '从 undo 快照恢复（点选即回滚）',
      onClick: () => send({ type: 'undo_list' }),
    },
    {
      label: '清空会话',
      hint: '上下文与历史文件，需确认',
      danger: true,
      onClick: () => send({ type: 'command', line: '/clear' }),
    },
    { sep: true },
    { label: '命令帮助', hint: '快捷键与斜杠命令', onClick: openHelpModal },
    {
      label: '退出 znaide',
      hint: '服务进程结束',
      danger: true,
      onClick: () => send({ type: 'command', line: '/quit' }),
    },
  ]);
}

/* ---------------- 会话侧栏 ---------------- */

function fmtAgo(ms) {
  if (!ms) return '—';
  const secs = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (secs < 60) return '刚刚';
  if (secs < 3600) return `${Math.floor(secs / 60)} 分钟前`;
  if (secs < 86400) return `${Math.floor(secs / 3600)} 小时前`;
  if (secs < 86400 * 30) return `${Math.floor(secs / 86400)} 天前`;
  return new Date(ms).toLocaleDateString();
}

function renderSessions(items) {
  state.sessions = Array.isArray(items) ? items : [];
  const q = sessSearch.value.trim().toLowerCase();
  sessList.textContent = '';
  let list = state.sessions.filter(
    (s) => !q || `${s.title || ''} ${s.note || ''} ${s.id}`.toLowerCase().includes(q),
  );
  // 当前会话可能还没写盘(新建/刚清空):始终占一行,避免切走后回不来
  if (state.session && !list.some((s) => s.id === state.session)) {
    const pin = {
      id: state.session,
      title: '当前会话（新）',
      note: null,
      mtime_ms: Date.now(),
      user_msgs: 0,
      headless: false,
      pinned: true,
    };
    if (!q || `当前会话（新） ${state.session}`.toLowerCase().includes(q)) {
      list = [pin, ...list];
    }
  }
  if (!list.length) {
    sessList.append(el('div', 'sess-empty', q ? '没有匹配的会话' : '还没有历史会话'));
    return;
  }
  for (const s of list) {
    const row = el('div', `sess${s.id === state.session ? ' current' : ''}`);
    const main = el('div', 'sess-main');
    main.append(el('div', 'sess-title', s.note || s.title || `会话 ${s.id.slice(0, 8)}`));
    const bits = [fmtAgo(s.mtime_ms), `${s.user_msgs} 条`];
    if (s.headless) bits.push('无头');
    main.append(el('div', 'sess-meta', bits.join(' · ')));
    row.append(main);

    const actions = el('div', 'sess-actions');
    const rename = el('button', 'mini', '✎');
    if (s.pinned) {
      row.append(main);
      sessList.append(row);
      continue;
    }
    rename.title = '备注（改名）';
    rename.addEventListener('click', (e) => {
      e.stopPropagation();
      openRenameModal(s);
    });
    const del = el('button', 'mini danger', '🗑');
    del.title = '删除会话';
    del.addEventListener('click', (e) => {
      e.stopPropagation();
      openDeleteModal(s);
    });
    actions.append(rename, del);
    row.append(actions);

    row.addEventListener('click', () => {
      if (s.id === state.session) return;
      if (state.busy) {
        notice('本轮还在执行:先按 Esc 中断,再切换会话。', 'warn');
        return;
      }
      if (s.cwd && s.cwd !== state.cwd) {
        // 会话自带工作目录:先切目录再恢复(不弹确认,切目录有提示)
        send({ type: 'set_cwd', path: s.cwd });
        send({ type: 'resume', id: s.id });
      } else if (!s.cwd) {
        notice('该会话未记录工作目录，继续使用当前目录。', 'warn');
        send({ type: 'resume', id: s.id });
      } else {
        send({ type: 'resume', id: s.id });
      }
    });
    sessList.append(row);
  }
}

sessSearch.addEventListener('input', () => renderSessions(state.sessions));
$('btn-new-session').addEventListener('click', () => {
  if (state.busy) {
    notice('本轮还在执行:先按 Esc 中断,再新建会话。', 'warn');
    return;
  }
  send({ type: 'new_session' });
});

// 用户向上滚就暂停自动跟随（流式输出/工具事件不再把视野推回底部）
stream.addEventListener(
  'scroll',
  () => {
    state.follow = nearBottom();
  },
  { passive: true },
);

const sidebarPref = localStorage.getItem('znaide-sidebar');
if (sidebarPref === '0') sidebar.hidden = true;
$('btn-sidebar').addEventListener('click', () => {
  sidebar.hidden = !sidebar.hidden;
  localStorage.setItem('znaide-sidebar', sidebar.hidden ? '0' : '1');
});

/* ---------------- 设置整页 ---------------- */

function openSettings(sec) {
  if (sec) state.settingsSection = sec;
  state.settingsOpen = true;
  send({ type: 'settings_open' });
}

function closeSettings() {
  state.settingsOpen = false;
  settingsEl.hidden = true;
}

function setInput(value, type = 'text') {
  const i = el('input', 'set-input grow');
  i.type = type;
  i.value = value ?? '';
  return i;
}

function setSelect(items, value) {
  const s = el('select', 'set-input grow');
  for (const it of items) {
    const o = el('option', '', it.label);
    o.value = it.value;
    o.selected = it.value === value;
    s.append(o);
  }
  return s;
}

function setButton(label, cls, fn) {
  const b = el('button', `btn ${cls}`.trim(), label);
  b.addEventListener('click', fn);
  return b;
}

function setRow(label, node) {
  const r = el('div', 'set-row');
  r.append(el('label', '', label), node);
  return r;
}

function setNote(text, cls = '') {
  return el('div', `set-note ${cls}`.trim(), text);
}

function setCard() {
  return el('div', 'set-card');
}

function setActions(...buttons) {
  const a = el('div', 'set-actions');
  a.append(...buttons);
  return a;
}

function renderSettings() {
  const s = state.settings;
  // 关闭后仍可能收到保存触发的 settings 刷新帧:不要把它重新弹出来
  if (!s || !state.settingsOpen) return;
  settingsEl.hidden = false;
  for (const b of document.querySelectorAll('.set-tab[data-sec]')) {
    b.classList.toggle('active', b.dataset.sec === state.settingsSection);
  }
  setBody.textContent = '';
  const fn = {
    model: sectionModel,
    network: sectionNetwork,
    persona: sectionPersona,
    skills: sectionSkills,
    memory: sectionMemory,
    mcp: sectionMcp,
    about: sectionAbout,
  }[state.settingsSection] || sectionModel;
  fn(s);
  setBody.scrollTop = 0;
}

let probeResultsEl = null;

function sectionModel(s) {
  if (!s.configured) {
    const w = setCard();
    w.append(
      el('div', 'set-sub', '首次运行：完成这三步'),
      setNote('1) 选服务商（本地 ollama 可留空 Key）\n2) 填端点/Key，点「探测模型」挑一个\n3) 保存并应用 —— 引擎会自动启动，本页随后关闭'),
    );
    setBody.append(w);
  }
  setBody.append(
    el('h1', 'set-h1', '服务商与模型'),
    el('p', 'set-sub', '保存后写进该服务商条目并立即重连；Key 只显示“已设置/未设置”，不会回传明文。'),
  );
  const c = setCard();
  const prov = setSelect(
    s.providers.map((p) => ({ value: p.name, label: p.name })),
    s.provider,
  );
  const model = setInput(s.model);
  const base = setInput(s.base_url);
  const key = setInput('', 'password');
  key.placeholder = s.api_key_set ? '已设置（留空保持不变）' : '未设置（本地端点可留空）';
  const clear = el('input');
  clear.type = 'checkbox';
  const clearWrap = el('label', 'set-check');
  clearWrap.append(clear, el('span', '', '清除已保存的 Key'));
  const win = setInput(s.context_window ?? 0, 'number');
  win.min = '0';
  const turns = setInput(s.max_turns ?? '', 'number');
  turns.min = '0';
  turns.placeholder = '默认 200；0 = 不限';
  const probeBox = el('div', 'set-list');
  probeResultsEl = probeBox;
  state.modelInput = model;
  const probeBtn = setButton('探测模型', 'ghost', () => {
    probeBox.textContent = '探测中…';
    send({ type: 'probe_models', base_url: base.value.trim(), api_key: key.value });
  });
  const modelRow = el('div', 'set-row');
  modelRow.append(el('label', '', '模型'), model, probeBtn);

  prov.addEventListener('change', () => {
    const p = s.providers.find((x) => x.name === prov.value);
    if (!p) return;
    model.value = p.model || '';
    if (p.base_url) base.value = p.base_url;
  });

  c.append(
    setRow('服务商', prov),
    modelRow,
    probeBox,
    setRow('端点', base),
    setRow('API Key', key),
    ...(s.api_key_env ? [setRow('Key 环境变量', el('span', 'mono dim', s.api_key_env))] : []),
    setRow('', clearWrap),
    setRow('上下文窗口', win),
    setRow('轮数上限', turns),
    setActions(
      setButton('保存并应用', 'primary', () => {
        send({
          type: 'save_model',
          provider: prov.value,
          model: model.value.trim(),
          base_url: base.value.trim(),
          api_key: key.value,
          clear_key: clear.checked,
          context_window: Number(win.value || 0),
          max_turns: turns.value === '' ? null : Number(turns.value),
        });
      }),
    ),
    setNote(
      (s.context_window_effective
        ? `实际生效：${s.context_window_effective} tokens`
        : '实际生效：未知（模型不在内置表里）') +
        (s.context_window ? '（配置固定值）' : '（自动查表）') +
        '\n填 0 = 自动；配置文件：' +
        s.config_path,
    ),
  );
  setBody.append(c);
}

function sectionNetwork(s) {
  setBody.append(
    el('h1', 'set-h1', '网络与请求'),
    el('p', 'set-sub', '代理是网络环境属性，不随服务商走；请求头只作用于模型请求。'),
  );
  const c = setCard();
  const mode = setSelect(
    [
      { value: 'auto', label: '跟随环境变量（auto）' },
      { value: 'direct', label: '强制直连（direct）' },
      { value: 'manual', label: '手动代理（manual）' },
    ],
    s.proxy.mode,
  );
  const url = setInput(s.proxy.url);
  url.placeholder = 'http://127.0.0.1:7897';
  url.disabled = s.proxy.mode !== 'manual';
  mode.addEventListener('change', () => {
    url.disabled = mode.value !== 'manual';
  });
  const retry = setInput(s.retry, 'number');
  retry.min = '0';
  retry.max = '8';
  const enabled = el('input');
  enabled.type = 'checkbox';
  enabled.checked = Boolean(s.headers_enabled);
  const enabledWrap = el('label', 'set-check');
  enabledWrap.append(enabled, el('span', '', '启用预设请求头'));

  let headers = (s.headers || []).map((h) => ({ name: h.name, value: h.value }));
  let providerHeaders = (s.provider_headers || []).map((h) => ({ name: h.name, value: h.value }));
  const providerPolicy = setSelect(
    [
      { value: 'inherit', label: '继承顶层' },
      { value: 'off', label: '本服务商不发' },
    ],
    s.provider_headers_enabled === false ? 'off' : 'inherit',
  );
  const providerList = el('div');
  const paintProvider = () => {
    providerList.textContent = '';
    providerHeaders.forEach((h, i) => {
      const row = el('div', 'header-row');
      const n = setInput(h.name);
      n.placeholder = '头名';
      const v = setInput(h.value);
      v.placeholder = '值（留空 = 删除顶层继承的同名头）';
      n.addEventListener('input', () => { h.name = n.value; });
      v.addEventListener('input', () => { h.value = v.value; });
      row.append(n, v, setButton('✕', 'ghost mini', () => {
        providerHeaders.splice(i, 1);
        paintProvider();
      }));
      providerList.append(row);
    });
  };
  paintProvider();

  const list = el('div');
  const paint = () => {
    list.textContent = '';
    headers.forEach((h, i) => {
      const row = el('div', 'header-row');
      const n = setInput(h.name);
      n.placeholder = '头名';
      const v = setInput(h.value);
      v.placeholder = '值（支持 ${VAR}）';
      n.addEventListener('input', () => {
        h.name = n.value;
      });
      v.addEventListener('input', () => {
        h.value = v.value;
      });
      row.append(n, v, setButton('✕', 'ghost mini', () => {
        headers.splice(i, 1);
        paint();
      }));
      list.append(row);
    });
  };
  paint();

  c.append(
    setRow('代理', mode),
    setRow('代理地址', url),
    setRow('弱网重试', retry),
    setRow('', enabledWrap),
    el('div', 'set-sep'),
    setRow('请求头', setButton('+ 添加', 'ghost', () => {
      headers.push({ name: '', value: '' });
      paint();
    })),
    list,
    el('div', 'set-sep'),
    el('div', 'set-sub', '本服务商额外请求头（叠加在顶层之上）'),
    setRow('策略', providerPolicy),
    setRow('请求头', setButton('+ 添加', 'ghost', () => {
      providerHeaders.push({ name: '', value: '' });
      paintProvider();
    })),
    providerList,
    setActions(
      setButton('保存', 'primary', () => {
        send({
          type: 'save_network',
          mode: mode.value,
          url: url.value.trim(),
          retry: Number(retry.value || 0),
          headers_enabled: enabled.checked,
          headers,
          provider_headers: providerHeaders,
          provider_headers_enabled: providerPolicy.value === 'off' ? false : null,
        });
      }),
    ),
    setNote('重试只重试一次模型调用，不会重跑工具；代理改动重启后生效。\n保留头（Authorization/Host/Content-Type…）会被忽略并提示。'),
  );
  setBody.append(c);
}

function sectionPersona(s) {
  setBody.append(
    el('h1', 'set-h1', '人格'),
    el('p', 'set-sub', '人格是系统级身份段，全局影响后续对话；写回配置，重启仍生效。'),
  );
  const c = setCard();
  const sel = setSelect(
    [{ value: '', label: '无（默认助手）' }, ...s.personas.map((n) => ({ value: n, label: n }))],
    s.persona,
  );
  const name = setInput('');
  name.placeholder = '人格名（新建或覆盖）';
  const body = el('textarea', 'set-input');
  body.placeholder = '人设正文（Markdown）';
  const fill = (n) => {
    name.value = n;
    body.value = (s.persona_texts || {})[n] || '';
  };
  const list = el('div', 'set-list');
  for (const n of s.personas) {
    const item = el('div', 'set-item');
    const main = el('div', 'set-item-main');
    main.append(el('div', 'set-item-title', n));
    item.append(main, setButton('编辑', 'ghost sm', () => fill(n)));
    list.append(item);
  }
  c.append(
    setRow('当前人格', sel),
    setActions(setButton('切换', '', () => send({ type: 'persona', name: sel.value }))),
    el('div', 'set-sep'),
    el('div', 'set-sub', '人格预设'),
    list,
    el('div', 'set-sep'),
    setRow('名字', name),
    body,
    setActions(
      setButton('新建', 'ghost', () => fill('')),
      setButton('保存人格', 'primary', () => {
        send({ type: 'save_persona_file', name: name.value.trim(), body: body.value });
      }),
    ),
    setNote('内置人格可在用户目录建同名文件覆盖；自定义保存在 ~/.znaide/personas/<名字>.md。'),
  );
  setBody.append(c);
}

function sectionSkills(s) {
  setBody.append(
    el('h1', 'set-h1', '技能'),
    el('p', 'set-sub', '内置只读；用户/项目技能可直接改正文（SKILL.md）。'),
  );
  const list = el('div', 'set-list');
  for (const sk of s.skills) {
    const item = el('div', 'set-item');
    const main = el('div', 'set-item-main');
    main.append(el('div', 'set-item-title', `/${sk.name}`));
    main.append(
      el('div', 'set-item-meta', `${sk.source}${sk.entry ? ' · 有入口脚本' : ''} · ${sk.desc || ''}`),
    );
    item.append(main);
    item.append(
      setButton('运行', 'ghost sm', () => openModal('skargs', { name: sk.name })),
    );
    if (sk.editable) {
      item.append(
        setButton('编辑', 'ghost sm', () => send({ type: 'skill_get', name: sk.name })),
      );
    }
    list.append(item);
  }
  const c = setCard();
  c.append(list);
  if (state.skillEditor) {
    const ed = state.skillEditor;
    const ta = el('textarea', 'set-input');
    ta.value = ed.body;
    c.append(
      el('div', 'set-sep'),
      setRow('技能', el('span', 'mono', ed.name)),
      setNote(ed.path + (ed.editable ? '' : '\n内置技能只读')),
      ta,
      setActions(
        setButton('保存技能', 'primary', () => {
          send({ type: 'skill_save', name: ed.name, body: ta.value });
        }),
      ),
    );
  }
  setBody.append(c);
}

function sectionMemory(s) {
  setBody.append(
    el('h1', 'set-h1', '记忆'),
    el('p', 'set-sub', 'Global 随用户走；Project 只对当前工作目录生效。'),
  );

  const add = setCard();
  const name = setInput('');
  const desc = setInput('');
  const mtype = setSelect(
    [
      { value: 'user', label: 'user（用户）' },
      { value: 'feedback', label: 'feedback（反馈）' },
      { value: 'project', label: 'project（项目）' },
      { value: 'reference', label: 'reference（参考）' },
    ],
    'user',
  );
  const scope = setSelect(
    [
      { value: 'project', label: 'Project（当前目录）' },
      { value: 'global', label: 'Global（全局）' },
    ],
    'project',
  );
  const body = el('textarea', 'set-input');
  body.placeholder = '记忆正文';
  add.append(
    el('div', 'set-sub', '新建记忆'),
    setRow('名字', name),
    setRow('描述', desc),
    setRow('类型', mtype),
    setRow('作用域', scope),
    body,
    setActions(
      setButton('保存记忆', 'primary', () => {
        send({
          type: 'memory_save',
          filename: '',
          name: name.value.trim(),
          description: desc.value.trim(),
          mtype: mtype.value,
          scope: scope.value,
          body: body.value,
        });
      }),
    ),
  );

  const list = el('div', 'set-list');
  for (const m of s.memories) {
    const item = el('div', 'set-item');
    const badge = el('span', `badge${m.scope === 'global' ? ' g' : ''}`, m.scope === 'global' ? 'G' : 'P');
    const main = el('div', 'set-item-main');
    main.append(el('div', 'set-item-title', m.name || m.filename));
    main.append(el('div', 'set-item-meta', `${m.mtype || '—'} · ${m.description || ''}`));
    item.append(badge, main);
    item.append(
      setButton('编辑', 'ghost sm', () => send({ type: 'memory_get', filename: m.filename, scope: m.scope })),
      setButton('删除', 'ghost sm', () =>
        openModal('memdel', { filename: m.filename, scope: m.scope, name: m.name || m.filename }),
      ),
    );
    list.append(item);
  }
  const listCard = setCard();
  listCard.append(el('div', 'set-sub', `已有记忆（${s.memories.length}）`), list);

  setBody.append(add, listCard);

  if (state.memoryEditor) {
    const ed = state.memoryEditor;
    const c = setCard();
    const n = setInput(ed.name);
    const d = setInput(ed.description);
    const b = el('textarea', 'set-input');
    b.value = ed.body;
    c.append(
      el('div', 'set-sub', `编辑记忆：${ed.filename}（${ed.scope === 'global' ? 'Global' : 'Project'}）`),
      setRow('名字', n),
      setRow('描述', d),
      b,
      setActions(
        setButton('保存修改', 'primary', () => {
          send({
            type: 'memory_save',
            filename: ed.filename,
            name: n.value.trim(),
            description: d.value.trim(),
            mtype: ed.mtype || 'user',
            scope: ed.scope,
            body: b.value,
          });
        }),
      ),
    );
    setBody.append(c);
  }
}

function sectionMcp(s) {
  setBody.append(
    el('h1', 'set-h1', 'MCP'),
    el('p', 'set-sub', '直接编辑 mcp.json；保存前会校验 JSON，改动在下次启动时连接。'),
  );
  const c = setCard();
  const ta = el('textarea', 'set-input');
  ta.style.minHeight = '260px';
  ta.value = s.mcp_text || '';
  c.append(
    setNote(s.mcp_path),
    ta,
    setActions(
      setButton('保存 mcp.json', 'primary', () => {
        send({ type: 'mcp_save', text: ta.value });
      }),
    ),
  );
  setBody.append(c);
}

function sectionAbout(s) {
  setBody.append(
    el('h1', 'set-h1', '关于与更新'),
    el('p', 'set-sub', `${s.product || '修凡AI全能助手'} V${s.gui_version || s.version}（基于 znaide 引擎 V${s.engine_version || '?'}） · ${s.build || ''}`),
  );
  const c = setCard();
  const theme = setSelect(
    [
      { value: 'dark', label: '深色（默认）' },
      { value: 'light', label: '浅色' },
    ],
    document.documentElement.dataset.theme === 'light' ? 'light' : 'dark',
  );
  theme.addEventListener('change', () => {
    document.documentElement.dataset.theme = theme.value;
    localStorage.setItem('znaide-theme', theme.value);
    applyHljsTheme();
  });
  c.append(
    setRow('外观', theme),
    el('div', 'set-sep'),
    setRow('版本', el('span', 'mono', `v${s.version}`)),
    setRow('配置', el('span', 'mono dim', s.config_path)),
    setRow('数据', el('span', 'mono dim', s.data_dir)),
    setRow('工作目录', el('span', 'mono dim', s.cwd)),
    setNote(s.update_notice || '启动时会静默检查新版本。'),
    setNote(state.updateResult || ''),
    setActions(
      setButton('检查更新', '', () => {
        state.updateResult = '正在检查…';
        send({ type: 'update_check' });
        renderSettings();
      }),
      setButton('立即更新', 'primary', () => {
        state.updateResult = '正在下载并安装…（完成后按提示重启）';
        send({ type: 'update_install' });
        renderSettings();
      }),
    ),
  );
  setBody.append(c);
}

/* ---------------- 设置事件 ---------------- */

for (const b of document.querySelectorAll('.set-tab[data-sec]')) {
  b.addEventListener('click', () => {
    state.settingsSection = b.dataset.sec;
    renderSettings();
  });
}
$('set-close').addEventListener('click', closeSettings);
$('btn-settings').addEventListener('click', () => openSettings());
chipUpdate.addEventListener('click', () => openSettings('about'));

function showUpdateChip(text) {
  state.updateNotice = text || '';
  chipUpdate.hidden = !state.updateNotice;
  if (text) chipUpdate.title = text;
}

function onEvent(frame) {
  switch (frame.kind) {
    case 'turn_started': {
      setBusy(true);
      // 上一回合结束后由前端 drain 的排队消息：轮到它变成新回合了
      if (state.queue.length) {
        const text = popQueue();
        syncQueue();
        if (text != null) renderUser(text);
      }
      break;
    }
    case 'reasoning_delta': {
      if (!state.reason) {
        hideEmpty();
        const d = el('details', 'reason');
        d.open = true;
        d.append(el('summary', '', '思考'));
        const body = el('div', 'reason-body');
        d.append(body);
        stream.append(d);
        state.reason = { node: d, body };
      }
      state.reason.body.textContent += frame.text;
      toBottom();
      break;
    }
    case 'text_delta': {
      state.reason = null;
      const cur = ensureAssistant();
      cur.text += frame.text;
      scheduleAssistantPaint();
      toBottom();
      break;
    }
    case 'tool_started':
      state.reason = null;
      toolCard(frame.name, frame.args, frame.idle_ms);
      break;
    case 'tool_output_delta': {
      const t = runningTool(frame.name);
      if (t) {
        t.buf += frame.delta || '';
        if (t.buf.length > 131072) t.buf = t.buf.slice(t.buf.length - 131072);
        paintToolOutput(t);
        if (nearBottom()) t.pre.scrollTop = t.pre.scrollHeight;
        toBottom();
      }
      break;
    }
    case 'tool_silent_alert': {
      const t = runningTool(frame.name);
      if (t) {
        t.card.classList.toggle('warn', frame.level === 1);
        t.card.classList.toggle('danger', frame.level >= 2);
      }
      break;
    }
    case 'tool_finished': {
      const t = runningTool(frame.name);
      if (t) {
        markToolDone(t, frame.ok, (Date.now() - t.started) / 1000);
        if (!t.buf && frame.output) {
          t.buf = frame.output;
          paintToolOutput(t);
        }
        t.card.open = !frame.ok && Boolean(frame.output);
      } else {
        notice(`${frame.ok ? '✓' : '✗'} ${frame.name}`, frame.ok ? '' : 'error');
      }
      toBottom();
      break;
    }
    case 'turn_finished': {
      const cur = state.current;
      if (cur) {
        if (frame.text) cur.text = frame.text;
        flushAssistantPaint(cur);
        cur.node.classList.remove('streaming');
        if (frame.truncated) notice('⚠ 本轮输出被截断。', 'warn');
      }
      state.current = null;
      state.reason = null;
      setBusy(false);
      // 回合结束仍有排队：交给宿主当成下一回合发出
      if (state.queue.length) send({ type: 'drain' });
      break;
    }
    case 'user_injected': {
      const text = popQueue(frame.text);
      renderUser(frame.text, true);
      if (text != null) syncQueue();
      break;
    }
    case 'llm_retrying': {
      // 撤掉本轮已吐出的半截流式输出，避免"半截 + 完整"拼接
      if (state.current) {
        state.current.text = '';
        state.current.node.innerHTML = '';
      }
      notice(`↻ 弱网重试 ${frame.attempt}/${frame.max}：${frame.reason}`, 'warn');
      break;
    }
    case 'notice':
      notice(frame.text);
      break;
    case 'usage': {
      state.tokens.input += frame.prompt || 0;
      state.tokens.output += frame.completion || 0;
      state.ctx.used = frame.prompt || 0; // 最近一次请求的输入 tokens 才是"占用"
      renderCtx();
      $('foot-tokens').textContent = `↓${state.tokens.input} ↑${state.tokens.output} tokens`;
      break;
    }
    case 'round_started': {
      $('foot-round').textContent = frame.limit ? `轮 ${frame.used}/${frame.limit}` : `轮 ${frame.used}`;
      break;
    }
    case 'session_info':
      setSession(frame.id);
      break;
    case 'history_loaded': {
      clearStream();
      // 换了会话:累计 token 与占用都是上一个会话的数字,留着会误导(占用等下一轮 usage)
      state.tokens = { input: 0, output: 0 };
      $('foot-tokens').textContent = '';
      state.ctx.used = null;
      renderCtx();
      notice(`已恢复历史会话（${frame.count} 条消息）。上下文未必与当时完全一致，必要时核对。`);
      renderMessages(frame.messages || []);
      break;
    }
    case 'context_cleared':
      clearStream();
      state.ctx.used = 0; // 真的空了
      state.tokens = { input: 0, output: 0 };
      $('foot-tokens').textContent = '';
      renderCtx();
      notice('上下文与历史已清空。', 'ok');
      break;
    case 'compaction_started':
      notice('正在压缩上下文…');
      break;
    case 'context_compacted': {
      clearStream();
      notice(`上下文已压缩：${frame.removed} 条旧消息折成摘要。`);
      renderMessages(frame.messages || []);
      break;
    }
    case 'persona_changed': {
      // 顶栏人格 chip 始终可见:没有 = "人格：无"
      applyStatus({ persona: frame.name || '' });
      break;
    }
    default:
      break;
  }
}

/* ---------------- 连接 ---------------- */

function setConn(kind, text) {
  connEl.className = `conn ${kind}`;
  connEl.textContent = text;
}

function connect() {
  if (!token) {
    setConn('off', '没有访问令牌');
    notice('没有访问令牌。请用 znaide-gui 启动时打印的完整链接打开本页（链接里带 token）。', 'error');
    return;
  }
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  const url = `${proto}//${location.host}/ws?token=${encodeURIComponent(token)}`;
  const ws = new WebSocket(url);
  state.ws = ws;

  ws.addEventListener('open', () => {
    state.reconnectMs = 500;
    setConn('on', '已连接');
  });
  ws.addEventListener('message', (e) => {
    let frame;
    try {
      frame = JSON.parse(e.data);
    } catch {
      return;
    }
    if (frame.type === 'hello') onHello(frame);
    else if (frame.type === 'event') onEvent(frame);
    else if (frame.type === 'ask') showAsk(frame);
    else if (frame.type === 'status') applyStatus(frame);
    else if (frame.type === 'sessions') renderSessions(frame.items);
    else if (frame.type === 'settings') {
      state.settings = frame.data;
      showUpdateChip(frame.data.update_notice || state.updateNotice);
      renderSettings();
    } else if (frame.type === 'personas') {
      state.personas = frame.items || [];
      if (!settingsEl.hidden && state.settingsSection === 'persona') renderSettings();
    } else if (frame.type === 'skill_content') {
      state.skillEditor = frame;
      if (!settingsEl.hidden) renderSettings();
    } else if (frame.type === 'memory_content') {
      state.memoryEditor = frame;
      if (!settingsEl.hidden) renderSettings();
    } else if (frame.type === 'update_result') {
      state.updateResult = frame.text;
      notice(frame.text, frame.kind === 'install' ? 'ok' : '');
      if (!settingsEl.hidden) renderSettings();
    } else if (frame.type === 'update_available') {
      showUpdateChip(frame.text);
    } else if (frame.type === 'model_list') {
      if (probeResultsEl && document.contains(probeResultsEl)) {
        probeResultsEl.textContent = '';
        if (!frame.models || !frame.models.length) {
          probeResultsEl.append(setNote('端点没有返回模型列表，可手输模型名。'));
        } else {
          const sel = setSelect(
            frame.models.map((m) => ({ value: m, label: m })),
            frame.models[0],
          );
          sel.addEventListener('change', () => {
            if (state.modelInput) state.modelInput.value = sel.value;
          });
          probeResultsEl.append(
            setNote(`探测到 ${frame.models.length} 个模型，选一个会填进上面的模型框：`),
            sel,
          );
        }
      }
    } else if (frame.type === 'paths') {
      if (state.at && frame.prefix === state.at.prefix) {
        paletteList = (frame.items || []).map((it) => ({
          name: `@${it.path}`,
          desc: it.dir ? '目录' : '文件',
          path: it.path,
          dir: it.dir,
        }));
        if (!paletteList.length) {
          hidePalette();
        } else {
          paletteIndex = 0;
          paintPalette();
          palette.hidden = false;
        }
      }
    } else if (frame.type === 'cwd') {
      renderCwd(frame.path);
    } else if (frame.type === 'cwd_pick_failed') {
      const p = window.prompt(`目录选择器不可用（${frame.message}），手动输入路径`, state.cwd);
      if (p && p.trim()) send({ type: 'set_cwd', path: p.trim() });
    } else if (frame.type === 'configured') {
      closeSettings();
      notice('✔ 配置完成，引擎已启动。', 'ok');
    }
    else if (frame.type === 'open_sessions') {
      sidebar.hidden = false;
      localStorage.setItem('znaide-sidebar', '1');
      sessSearch.focus();
    }
    else if (frame.type === 'undo_list') openUndoModal(frame.items);
    else if (frame.type === 'command_line') {
      if (frame.kind === 'output') renderCommandOutput(frame.text);
      else notice(frame.text);
    } else if (frame.type === 'confirm') openModal('clear', frame);
    else if (frame.type === 'shutdown') {
      state.closing = true;
      setBusy(false);
      setConn('off', '服务已退出');
      notice('znaide 已退出。', 'ok');
      try {
        ws.close();
      } catch {
        // 已经关了
      }
    } else if (frame.type === 'error') notice(frame.message || '未知错误', 'error');
  });
  ws.addEventListener('close', () => {
    if (state.ws !== ws) return;
    state.ws = null;
    if (state.closing) {
      setConn('off', '服务已退出');
      return;
    }
    setConn('off', '连接断开，正在重连…');
    setBusy(false);
    const delay = state.reconnectMs;
    state.reconnectMs = Math.min(state.reconnectMs * 2, 5000);
    setTimeout(connect, delay);
  });
  ws.addEventListener('error', () => {
    // close 会紧接着触发，重连与提示都在那里处理
  });
}

function send(obj) {
  if (!state.ws || state.ws.readyState !== WebSocket.OPEN) {
    notice('还没连上本地服务，稍等重连后再试。', 'warn');
    return false;
  }
  state.ws.send(JSON.stringify(obj));
  return true;
}

/* ---------------- 输入 ---------------- */

function autosize() {
  input.style.height = 'auto';
  input.style.height = `${Math.min(input.scrollHeight, 200)}px`;
}

function submit() {
  hidePalette();
  const text = input.value.trim();
  if (!text) return;

  // /config 在 GUI 里落到设置整页(宿主没有这条命令,TUI 是在界面里先截住的)
  if (text === '/config') {
    input.value = '';
    autosize();
    openSettings();
    return;
  }

  if (text.startsWith('/')) {
    // 命令语义在宿主(host::command),前端只负责送进去并渲染回吐的行
    if (state.busy) {
      notice('本轮还在执行:先等它结束(或 Esc 中断)再执行命令。', 'warn');
      return;
    }
    if (!send({ type: 'command', line: text })) return;
    renderCommandEcho(text);
    input.value = '';
    autosize();
    return;
  }

  if (state.busy) {
    if (send({ type: 'queue', text })) {
      pushQueue(text);
      input.value = '';
      autosize();
    }
    return;
  }
  if (!send({ type: 'prompt', text })) return;
  renderUser(text);
  state.follow = true;
  toBottom(true);
  setBusy(true);
  input.value = '';
  autosize();
}

input.addEventListener('input', () => {
  autosize();
  updatePalette();
  updateAtPalette();
});
input.addEventListener('keydown', (e) => {
  if (!palette.hidden) {
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      paletteIndex = (paletteIndex + 1) % paletteList.length;
      paintPalette();
      return;
    }
    if (e.key === 'ArrowUp') {
      e.preventDefault();
      paletteIndex = (paletteIndex - 1 + paletteList.length) % paletteList.length;
      paintPalette();
      return;
    }
    if (e.key === 'Escape') {
      e.preventDefault();
      hidePalette();
      return;
    }
    if (e.key === 'Enter' && !e.shiftKey && !e.isComposing && paletteIndex >= 0) {
      e.preventDefault();
      pickPalette(paletteList[paletteIndex]);
      return;
    }
  }
  if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) {
    e.preventDefault();
    submit();
  } else if (e.key === 'Escape') {
    e.preventDefault();
    if (state.modal) return; // 交给全局 Esc
    if (state.busy) send({ type: 'cancel' });
  }
});

btnSend.addEventListener('click', submit);
btnStop.addEventListener('click', () => send({ type: 'cancel' }));

document.addEventListener('keydown', (e) => {
  if (e.key !== 'Escape') return;
  if (!settingsEl.hidden && !state.modal) {
    e.preventDefault();
    closeSettings();
    return;
  }
  if (!state.modal) return;
  e.preventDefault();
  if (state.modal.kind === 'permission') answerPermission(false, false);
  else if (state.modal.kind === 'rounds') answerRounds(false);
  else closeModal();
});

/* ---------------- 右键菜单(三端统一,页面内绘制) ---------------- */

let ctxMenu = null;

function hideCtx() {
  if (ctxMenu) {
    ctxMenu.remove();
    ctxMenu = null;
  }
}

function insertAtCursor(target, text) {
  if (target.tagName === 'INPUT' || target.tagName === 'TEXTAREA') {
    const start = target.selectionStart ?? target.value.length;
    const end = target.selectionEnd ?? start;
    target.value = target.value.slice(0, start) + text + target.value.slice(end);
    target.selectionStart = target.selectionEnd = start + text.length;
    target.dispatchEvent(new Event('input', { bubbles: true }));
  } else {
    document.execCommand('insertText', false, text);
  }
}

document.addEventListener('contextmenu', (e) => {
  e.preventDefault();
  hideCtx();
  const target = e.target;
  const editable =
    target &&
    (target.tagName === 'TEXTAREA' ||
      target.tagName === 'INPUT' ||
      target.isContentEditable);
  const sel = window.getSelection();
  const hasSel = editable
    ? (target.selectionStart ?? 0) !== (target.selectionEnd ?? 0)
    : Boolean(sel && !sel.isCollapsed);

  const items = [];
  if (hasSel) {
    items.push([
      '复制',
      async () => {
        try {
          if (editable) document.execCommand('copy');
          else await navigator.clipboard.writeText(sel.toString());
        } catch {
          document.execCommand('copy');
        }
      },
    ]);
  }
  if (editable) {
    items.push([
      '粘贴',
      async () => {
        try {
          const text = await navigator.clipboard.readText();
          insertAtCursor(target, text);
        } catch {
          /* 没有剪贴板权限就保持原样 */
        }
      },
    ]);
    items.push([
      '全选',
      () => {
        if (target.select) target.select();
        else document.execCommand('selectAll');
      },
    ]);
  } else if (hasSel) {
    items.push(['全选', () => document.execCommand('selectAll')]);
  }
  if (!items.length) return;

  const menu = el('div', 'ctx-menu');
  for (const [label, fn] of items) {
    const b = el('button', 'ctx-item', label);
    b.addEventListener('click', async () => {
      hideCtx();
      await fn();
    });
    menu.append(b);
  }
  document.body.append(menu);
  menu.style.left = `${Math.min(e.clientX, window.innerWidth - menu.offsetWidth - 8)}px`;
  menu.style.top = `${Math.min(e.clientY, window.innerHeight - menu.offsetHeight - 8)}px`;
  ctxMenu = menu;
});

document.addEventListener('mousedown', (e) => {
  if (ctxMenu && !ctxMenu.contains(e.target)) hideCtx();
});
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape') hideCtx();
});
stream.addEventListener('scroll', hideCtx, { passive: true });

/* ---------------- 启动 ---------------- */

$('chip-cwd').addEventListener('click', () => {
  send({ type: 'pick_cwd' });
});
$('chip-mode').addEventListener('click', openModeMenu);
$('chip-model').addEventListener('click', openProviderMenu);
$('chip-persona').addEventListener('click', openPersonaMenu);
$('btn-more').addEventListener('click', openMoreMenu);
$('btn-compact').addEventListener('click', () => send({ type: 'compact' }));

connect();
autosize();
applyHljsTheme();
input.focus();
