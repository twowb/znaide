# znaide

**[English](README.en.md) · 中文**

**跑在终端里的"无所不能"通用 AI 助手。**

用自然语言下达指令,znaide 自主完成:改文件、跑命令、查资料、整理数据、写文档……编程只是应用场景之一。它参考 qwen-code 的 agent 架构思想,用 Rust 从零实现:**单个静态二进制、零 Node 依赖、中文交互、本地模型与云 API 通吃**。

> **源码与获取**:本工具**以 AGPL-3.0 开源**(仓库:[github.com/twowb/znaide](https://github.com/twowb/znaide)),自用/学习/开源衍生免费;**商用或闭源集成需商业授权**,见 `COMMERCIAL.md`。不想编译就直接下二进制:[GitHub Releases](https://github.com/twowb/znaide/releases) 按平台取 `znaide-<平台>-v<版本>`(Linux/macOS/Windows/Android-Termux),`--version` 可查看版本。

```
  znaide                              # 交互模式(推荐,首次运行会引导配置)
  znaide -p "把 ~/Downloads 里的 zip 按日期归档"   # 无头模式直接执行任务
```

## 特性

- **通用 Agent**:内置工具 + agent 循环,自主"侦查 → 执行 → 汇报",不只限于写代码
- **多模型后端**:OpenAI 兼容协议一网打尽——本地 ollama / vLLM,云端 DeepSeek / 通义(DashScope)/ OpenRouter 等,任意 `base_url + key + model` 均可
- **配置向导**:首次运行分步引导——选服务商 → **自动查询可用模型列表**(`/models`)→ 选/输入模型 → 填 Key → **验证连通成功后才解锁使用**;之后随时 `/config` 修改并立即生效
- **工具集**:文件读写/编辑(写前自动快照)、目录列举、glob、正则搜索、shell 命令(带超时)、网页抓取、长期记忆
- **安全四档**:**询问** ask(默认,写文件/命令先确认)/ **编辑放行** acceptEdits(文件修改自动放行)/ **全自动** bypassPermissions / **超级** yolo(一切放行含危险命令,后果自负),`Shift+Tab` 随时循环切换;前两档危险命令(`rm -rf /`、dd 写盘、git push --force 等)任何模式都拦截,**超级 yolo 连黑名单也跳过**
- **undo 回滚**:每次改文件前自动快照,`/undo` 一键回滚,不依赖 git
- **长期记忆**:跨会话记住你的环境与偏好,会话启动自动注入记忆摘要
- **MCP 支持**:配置 `~/.znaide/mcp.json` 即可接入任意 MCP server 的工具
- **会话历史**:每轮消息落盘 JSONL,`/resume` 随时恢复继续
- **技能(Skills)**:可被模型自主调用、也可 `/技能名` 手动触发的能力包(说明书 + 可选入口脚本),三层存放随时热扩展
- **`@文件` 引用**:输入 `@路径` 自动把文件内容/目录清单注入给模型;`@"含空格路径"` 可引用任意文件;输入时有路径补全(见下文)

## 快速开始

### 方式一:直接编译(需要 Rust 工具链)

```bash
cargo build --release
# 产物:target/release/znaide
```

### 方式二:make 跨平台构建(推荐)

```bash
make build        # 编译本机平台(自动检测)
make build-linux  # Linux x86_64(musl 全静态)
make build-linux-arm64  # Linux aarch64(musl 全静态)
make build-windows      # Windows x86_64(CRT 静态)
make build-macos        # macOS x86_64(zig 交叉编译)
make build-macos-arm64  # macOS aarch64(zig 交叉编译)
make build-android      # Android aarch64(NDK bionic 动态,Termux 用;NDK_ROOT 可覆盖)
make build-all          # 六平台一次构建(缺工具链自动跳过;android 需 NDK)
```

所有构建均为 `--release` 最小化产物(opt-level=z + LTO + 剥符号),产物在 `dist/` 下。跨平台构建细节见 [Makefile](./Makefile)。

> 首次交叉编译可能提示 `rustup target add <triple>`(联网下载);Linux musl 静态需要 musl 交叉 gcc(Ubuntu:`sudo apt install musl-tools`);macOS 走 **zig 万能交叉编译**(无需 osxcross/SDK),需要 `~/.cargo/bin` 下的 `zig-cc-*-darwin` wrapper 与 `~/.cargo/darwin-stubs/libiconv.tbd`(说明见 Makefile 与 `.cargo/config.toml`)。

## 首次运行配置向导

首次启动(尚无 `~/.znaide/config.json`)时,交互模式会自动打开配置向导:

1. **选择服务商**:内置 ollama / dashscope / deepseek / openrouter / zai,或选 custom 手输端点
2. **选择模型**:程序自动请求该服务商的 `/models` 接口拉取**真实可用模型**,↑↓ 选择;若查询失败或想自定义,按 `m` 直接输入
3. **填 API Key**:按 `e` 输入(本地服务如 ollama 可留空)
4. **验证并解锁**:按 `s` 真实发一条请求验证——**验证通过才保存配置并允许使用**;失败则提示修改

之后随时输入 `/config` 重新打开向导改服务商/模型/Key,保存即生效(不中断当前会话)。

## 使用

### 交互模式

```
  znaide
  znaide --resume <会话ID|片段>   # 启动即恢复历史会话(ID 见底部状态栏/退出统计)
```

启动显示品牌横幅;`/quit` 或 `/exit` 退出(或 Ctrl+C),退出时在普通终端打印**本次会话统计**(时长/消息数/工具调用/Token/undo 快照/会话 ID)。启动后输入指令即可。常用按键:

| 按键 | 作用 |
|---|---|
| `Enter` | 发送 |
| `Shift+Enter`(或 `Alt+Enter` / `Ctrl+J`) | 换行 |
| `Shift+Tab` | 循环切换权限模式(询问 → 编辑放行 → 全自动 → 超级) |
| `↑` / `↓` | 消息区上/下滚动 3 行 |
| `PageUp` / `PageDown` | 消息区上/下滚动 15 行 |
| `Home` / `End` | 消息区直接到**最顶** / **最底** |
| `Esc` | 中断生成 / 取消确认 |
| `Ctrl+C` | 退出 |

**输入补全(实时)**:输入 `/` 弹出命令菜单(内置命令 + 已安装技能),输入 `@` 弹出文件/目录补全(`@dir/` 可逐层深入,`@~/` 到家目录)——候选随输入实时过滤,首个候选同时在输入框内以**淡色 ghost** 预览。菜单打开时:`↑` / `↓` 选择,**`Tab` 或 `Enter` 采纳**(采纳后需再按一次 `Enter` 才会发送,避免误触),`Esc` 关闭。含空格/标点的文件名采纳时自动转成 `@"带空格路径"` 形式(与 @ 引用解析一致),能引用任意文件。

**鼠标**:消息区内**滚轮**直接滚动;内容超出时消息区右侧会出现**滚动条**,可**点击任意位置跳转**,或**按住拖动**实时滚动(需终端支持鼠标事件;Linux 与 Windows 的 Windows Terminal / conhost 均可)。

**工具调用卡片**会实时显示 `⏳ → ✓/✗`(执行中图标是脉动动画),标题下方浅灰小字直接展示**正在调用的内容**——命令类显示原文(`$ ls -la`)、其它工具显示参数,执行中就能看清 AI 在干什么、要不要按 `Esc` 拦下,完成后也保留(结果输出在其下);模型自主调用技能时卡片显示为 `skill「技能名」`;需要写文件或执行命令时弹出确认框:`y` 允许一次 / `a` 本次会话都允许 / `n` 拒绝;破坏性操作(如 `/clear` 清空会话)弹独立确认框:`y` 确认 / `n` 或 `Esc` 取消。命令与工具输出的卡片预览**保留 ANSI 颜色**(所有上屏文本均经控制字节净化,不会污染画面);鼠标被接管后需复制时按住 `Shift` 拖动选择。

**执行中的命令还有一块实时输出区**:卡片下方划出 `────── 实时输出 ──────`,像 `tail -f` 一样滚动显示**最新 10 行**输出(ANSI 颜色保留);消息区自动贴底跟随,你上翻看历史时暂停跟随、视野不会被新输出推走,回到底部恢复;命令结束后区域收回,卡片回落为结果预览。

**卡片带动态计时**:执行中的命令显示 `⏳ run_shell_command · 12s · 预算 4min`,秒级跳动,完成后显示总用时。AI 执行命令时可传 `idle_ms` 作为**预估完成时长**(宁大勿小;不传则不设预算、正常命令零打扰):命令跑超预估(另有约 20%、最多 60s 宽限)仍未结束就被终止,终止时把运行时长与死前输出反馈给 AI,由它自行决定**加大预算重试、拆步或换路**;命令连续 90s 没动静会被判卡死;长时间无输出时卡片先变黄、临近终止变红,**超出预算进入宽限期时卡片会明示"已超预算,宽限剩 Xs"**,终止原因与进展都会回给 AI,不会盲目重跑。

执行中按 `Esc` 可随时中断:正在跑的工具立即停,同批还没来得及执行的动作会在历史里记成「未执行」,会话历史始终保持完整——中断后继续对话、或日后 `--resume` 恢复,都能无缝接着聊,不会出现恢复后无法对话的情况。

**底部状态栏**从左到右显示:`权限模式(中文短名:询问 / 编辑放行 / 全自动 / 超级)[| 人格(有人格时显示,空 = 默认助手)] | 模型 | 状态(就绪/工作中…/压缩中…;执行=流动光条、压缩=收纳推进条动画)[| token 统计:in 输入 · out 输出 · Σ 会话总计(回复流式中 out/Σ 带 ≈ 实时估算)][| ctx ▓▓▓▓▓░░░░░ 34% 上下文占用条(占模型窗口比例,窗口已知即常驻、0% 也显示;70% 变黄 / 90% 变红并提示可 /compact;刚压缩后从 0% 重新计起)] · 会话 <id>`。

**输入框与输出框边框颜色 = 当前权限模式**:询问=绿 / 编辑放行=天蓝 / 全自动=紫 / 超级(YOLO)=红(工作中输入框禁用时变灰,输出框常显);`Shift+Tab` 切换后边框立即变色。会话 ID 启动即显示,与本会话历史文件同名(`~/.znaide/sessions/<id>.jsonl`),可配合 `/resume` 用 ID 片段精确定位恢复。

### Slash 命令

| 命令 | 作用 |
|---|---|
| `/help` | 显示帮助 |
| `/skills` | 列出已安装技能(含扫描告警) |
| `/config` | 打开配置面板(随时改 provider/模型/端点/Key,立即生效) |
| `/undo` `/undo <序号>` | 列出快照 / 回滚到指定版本 |
| `/resume` `/resume <序号\|片段>` | **无参**:打开全屏**会话管理窗口**(「历史会话/长期记忆」页签:↑↓ 选择、空格多选、`d` 批量删除、`n` 改备注、`/` 过滤、Enter 恢复/查看,当前会话禁删);**带参**:直接恢复该历史会话(`[无头]` = 命令行 `-p` 产生) |
| `/clear` | 清空会话上下文与历史文件(需确认,不可恢复) |
| `/compact` | 压缩上下文:旧对话收敛为一段中文摘要,上下文回到低位(历史档案完整保留)。压缩过程不可中断:状态栏「压缩中…」+ 收纳推进条动画,消息区忙行提示约需数秒~数十秒,完成/失败均有结果提示 |
| `/update` | 检查并更新到最新版本(启动时自动检查一次,发现新版会提示;也可用 `znaide --update`)。**双源自动回退**:默认 GitHub,网络不通自动切 Gitee 镜像源(国内无需代理);GitHub 源可设 `HTTPS_PROXY` 加速。Linux/macOS 替换后下次启动生效,Windows 退出程序后自动完成替换 |
| `/persona` | 全局人格:列表 / 切换(`/persona <名字>`),`/persona 无` 关闭。影响之后所有对话的表达与视角,写回配置持久生效;工作守则与安全边界不受人格影响 |
| `/quit` `/exit` | 退出程序(退出时打印本次会话统计;Ctrl+C 同效) |
| `/技能名 [参数]` | 手动触发已安装的技能(见下文"技能") |

### 人格(persona)

**全局人设**:`/persona` 查看/切换内置人格(毒舌损友 / 耐心老师 / 热血极客 / 极简冷淡风),`/persona 无` 关闭(默认)。切完**写回 config 持久生效**——之后所有对话(问答/汇报/建议)都带着这个人格,重启也在,直到再切。底部状态栏常驻显示当前人格,随时知道"是谁在说话"。人格影响表达与视角;工作守则与安全边界(权限、危险命令拦截、工具纪律)作为底层约束不被覆盖。

**人格就是一份 Markdown,随便改**:人格文案放在仓库 `personas示例/`(编译期内嵌,单源),启动物化为 `~/.znaide/personas/<名字>.md`——**内置的四种人格也以文件形式躺在那里**,打开就能看、随手改两句措辞(比如让毒舌损友少损一点),改完重启即生效;**删掉某个人格文件就恢复内置默认**;想要新人格,往目录里放一个 `<名字>.md` 就行,不用改代码。想收藏别人的?复制一份 md 进目录即可。

### 技能(Skills)

**技能 = 可被模型自主调用、也可手动 `/名字` 触发的能力包**。模型判断任务匹配某个技能时,会调用 `skill` 工具并把技能名带进来;系统把说明书正文(渲染好 `{args}`)+ 可选入口脚本的输出注入上下文,模型按说明用基础工具逐步完成。技能不新增权限面——所有落地动作仍走权限三档、危险命令黑名单与 undo 快照。

存放三层(同名按 **项目 > 用户 > 内置** 覆盖):

```
内置(开箱即用)                       # archive-downloads / clean-junk / weekly-report
~/.znaide/skills/<名字>/SKILL.md       # 用户级(推荐)
.znaide/skills/<名字>/SKILL.md         # 项目级(随仓库分发,内容视为不可信输入)
```

三个示例技能已**编译进二进制、开箱即有**(`/skills` 可见、模型可自主调用),内容物化在 `~/.znaide/builtin-skills/`(改了示例源目录即改内置;启动自动补齐缺失文件,已改动的不覆盖)。发布不再单独附带技能包。

格式(`SKILL.md`,frontmatter 简子集):

```markdown
---
description: 把 ~/Downloads 里的压缩包按日期归档(给模型判断何时用)
disable-model-invocation: true   # 可选:true 则不出现在模型调用列表,只能手动 /名字
entry: scripts/run.sh            # 可选:调用时自动执行一次,stdout 并入上下文
---
正文:教模型怎么做。支持 {args} 占位,可指示"先读 scripts/checklist.md"。
```

- **模型调用**:技能暴露为一个 `skill` 工具,可用技能名在参数 `enum` 里;`disable-model-invocation: true` 的技能不出现在列表
- **手动触发**:`/技能名 参数...`;无入口脚本时正文直接作为本回合指令(旧自定义命令同款),有入口脚本时在 agent 侧统一执行(命令需按权限确认)
- **入口脚本 entry**:相对技能目录的脚本,**跨平台**:macOS/Linux 用 `.sh`(bash 执行,不依赖 +x);Windows 上引擎自动找同名 `.ps1` → `.bat` → `.cmd`(用 PowerShell/`call` 执行),都没有才退回 `.sh` 并在结果里说明。`{args}` 经命令行参数传入,环境变量 `SKILL_DIR` 指向技能目录。**正文也应写平台分支命令**(如移动:`mv` ↔ Windows `move`/`Move-Item`),不要只写自己机器的写法。**项目级技能也允许 entry,但执行前会走权限确认**,警惕第三方仓库自带的 SKILL.md/脚本

示例一(归档,跨平台:入口放 `scripts/snapshot.sh`,同目录再放一份 `scripts/snapshot.ps1` 供 Windows 用):

```markdown
---
description: 把系统下载目录里的压缩包按类型归档
entry: scripts/snapshot.sh
---
入口输出按当前平台给出快照。来源目录:Linux/macOS 默认 ~/Downloads,
Windows 默认 %USERPROFILE%\Downloads。归档到 ~/archives(%USERPROFILE%\archives)。
移动命令按平台选择:macOS/Linux 用 mv;Windows 用 move 或 PowerShell Move-Item。
```

示例二(写周报,无入口):

```markdown
---
description: 根据 git 提交记录写一段中文周报
---
先 cd 到目标仓库执行 git log --oneline -30 查看近期提交,
再按"做了什么 → 影响 → 下一步"总结成一段简洁中文周报。
```

> 旧版 `~/.znaide/commands/*.md` 会在启动时自动迁移为 `~/.znaide/skills/<名字>/SKILL.md`(保持仅手动触发),无需手动处理。

### 无头模式(脚本 / CI)

```bash
# 单次执行,默认 ask 模式(写文件/命令会被拒绝,按需放开)
znaide -p "看看这个目录里有什么"

# 允许自动改文件(命令仍需确认)
znaide -p "把 README 里的 X 改成 Y" --permission acceptEdits

# 全自动执行
znaide -p "把 ~/Downloads 里的 zip 按日期归档" --permission bypassPermissions

# 临时指定模型/服务商(不改配置文件)
znaide -p "你好" --provider deepseek
znaide -p "你好" --model qwen3:8b --base-url http://localhost:11434/v1
```

## 配置

### 配置文件 `~/.znaide/config.json`

```json
{
  "provider": "ollama",
  "model": "qwen3:8b",
  "base_url": "http://localhost:11434/v1",
  "api_key": "",
  "providers": {
    "ollama": {
      "base_url": "http://localhost:11434/v1",
      "model": "qwen3:8b"
    },
    "dashscope": {
      "base_url": "https://dashscope.aliyuncs.com/compatible-mode/v1",
      "model": "qwen-plus",
      "api_key_env": "DASHSCOPE_API_KEY"
    },
    "deepseek": {
      "base_url": "https://api.deepseek.com/v1",
      "model": "deepseek-chat",
      "api_key_env": "DEEPSEEK_API_KEY"
    }
  }
}
```

字段说明:

- `provider`:当前启用的服务商名(须在 `providers` 里,或使用内置预设)
- `model` / `base_url` / `api_key`:可覆盖 provider 预设的顶层快捷字段
- `providers`:服务商预设表。`api_key_env` 表示从该环境变量读取 Key(推荐,避免明文);也可以直接写 `api_key`
- `context_window`(可选):当前模型的上下文窗口 token 数。不设置时按模型名匹配内置表(2026-09 检索:qwen3→40k、qwen-plus→1M、deepseek-v4→1M、gpt-5→400k、claude→200k 等),未命中默认 32k;本地模型(ollama)请与运行时的 `num_ctx` 保持一致,否则上下文占用条不准

API Key 也可以完全不进配置文件,用环境变量提供:`DASHSCOPE_API_KEY`、`DEEPSEEK_API_KEY`、`ZAI_API_KEY`、`OPENROUTER_API_KEY` 等,向导会自动读取。

### 环境变量

配置优先级:**命令行参数 > 环境变量 > config.json > 内置预设**。

| 环境变量 | 作用 |
|---|---|
| `ZNAIDE_PROVIDER` | 选择服务商预设 |
| `ZNAIDE_MODEL` / `OPENAI_MODEL` | 模型名 |
| `ZNAIDE_BASE_URL` / `OPENAI_BASE_URL` | OpenAI 兼容端点 |
| `ZNAIDE_API_KEY` / `OPENAI_API_KEY` | API Key |
| `ZNAIDE_DATA_DIR` | 覆盖数据目录(默认 `~/.znaide`,测试/多实例用) |

### 数据目录结构 `~/.znaide/`

```
~/.znaide/
├─ config.json      配置(provider / model / base_url / api_key)
├─ mcp.json         MCP server 配置(可选)
├─ skills/          技能(skills/<名字>/SKILL.md,可带 scripts/)
├─ sessions/        会话历史 *.jsonl(可有同名 *.meta.json 备注)
├─ memories/        长期记忆(*.md + MEMORY.md 索引)
└─ undo/            写文件前快照(manifest.jsonl + files/)
```

## MCP(Model Context Protocol)

编辑 `~/.znaide/mcp.json`:

```json
{
  "mcpServers": {
    "filesystem": { "command": "node", "args": ["/path/to/server.js"] }
  }
}
```

启动后 znaide 自动连接并把 MCP 工具并入工具箱(工具名形如 `mcp__<server>__<tool>`),模型可直接调用;MCP 工具按"写操作"对待,**询问**模式下会先征求确认。

## 项目结构

```
znaide/
├─ crates/
│  ├─ core/     引擎:agent 循环、LLM 客户端(OpenAI 兼容)、工具、权限、技能(skills)、记忆、会话、undo、MCP、配置(纯逻辑,零终端依赖)
│  ├─ tui/      交互界面(ratatui):对话区、输入、配置向导、确认框、工具卡片、滚动条与鼠标
│  └─ cli/      入口:交互 / -p 无头 / 参数解析
├─ docs/         方案与设计文档(plan-v1.md、plan-skills.md)
└─ Makefile      跨平台构建
```

## 开发

```bash
cargo build              # 调试构建
cargo test               # 单元 + 集成测试(含本地 mock 端到端)
cargo test -p znaide-tui # TUI 相关(配置向导、按键逻辑)
```

