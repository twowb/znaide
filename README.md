# znaide 使用说明

znaide 是一个跑在终端里的通用 AI 助手:你用自然语言下指令,它自主完成改文件、跑命令、查资料、整理数据等任务;编程只是它的应用场景之一。本说明面向拿到分发包(znaide + 可选 skills 示例)的终端用户。

- 平台:Linux / macOS / Windows(单文件二进制,无需安装运行时)
- 语言:中文交互
- 模型:任意 OpenAI 兼容端点(本地 ollama/vLLM 或云端 DeepSeek、通义 DashScope、OpenRouter 等)

---

## 1. 快速开始

1. **解压分发包**,把对应平台的 `znaide`(Windows 为 `znaide.exe`)放到 `PATH` 目录,或直接使用其绝对路径。

2. **首次启动**:在终端运行

   ```
   znaide
   ```

   程序发现还没有配置,会自动打开**配置向导**(见第 3 节)。配置成功前对话功能不可用。

3. **配置成功后**,输入框输入指令回车即可开始对话。随时输入 `/help` 查看帮助。

> 数据目录:`~/.znaide`(Windows:`%USERPROFILE%\.znaide`)。会话、记忆、技能、undo 快照都存在这里,卸载程序不影响这些数据。

---

## 2. 交互界面

启动后屏幕分四块:

```
┌─ 头部:品牌 + 当前工作目录(cwd)
├─ 消息区:对话记录 / 工具卡片 / 系统提示(可滚动,超长时右侧有滚动条)
├─ 输入区:❯ 提示符 + 多行输入(空输入时 ❯ 也常驻)
└─ 状态栏:权限模式 | 模型 | 状态 · token 统计 · ctx 占用条 · 会话 <id>(各字段见下)
```

- **当前工作目录(cwd)**:程序在哪个目录启动,agent 就以它为默认作用域(相对路径、默认搜索目录、命令执行目录)。想换项目就换目录重新启动。
- **会话 ID**:状态栏末尾显示,与会话历史文件同名(`~/.znaide/sessions/<id>.jsonl`),可用 `/resume` 按 ID 片段恢复。
- **状态栏字段**(从左到右):
  - **权限模式**:中文短名 询问 / 编辑放行 / 全自动 / 超级(`Shift+Tab` 切换,输入框与输出框边框颜色随模式,见 5.1);
  - **模型**:当前使用的模型名;
  - **状态**:就绪 / 工作中…(流动光条动画)/ 压缩中…(收纳推进条动画,见第 6 节 `/compact`);
  - **token 统计**:`in 输入 · out 输出 · Σ 会话总计`;回复流式输出中 `out`/`Σ` 带 `≈` 为实时估算;
  - **ctx 上下文占用条**:`▓▓▓░░ 34%`,当前上下文占模型窗口的比例(窗口大小见 13.1 的 `context_window`)。窗口已知即**常驻显示**(0% 也显示);70% 变黄、90% 变红并提示可 `/compact`;刚压缩后从 0% 重新计起。

### 2.1 常用按键

| 按键                                     | 作用                                              |
| ---------------------------------------- | ------------------------------------------------- |
| `Enter`                                  | 发送                                              |
| `Shift+Enter`(或 `Alt+Enter` / `Ctrl+J`) | 换行                                              |
| `Shift+Tab`                              | 循环切换权限模式(询问 → 编辑放行 → 全自动 → 超级) |
| `←` / `→`                                | 输入框内移动光标                                  |
| `Delete` / `Backspace`                   | 删除光标处 / 光标前字符                           |
| `↑` / `↓`                                | 消息区上/下滚动 3 行                              |
| `PageUp` / `PageDown`                    | 消息区上/下滚动 15 行                             |
| `Home` / `End`                           | 消息区直接到最顶 / 最底                           |
| `Esc`                                    | 中断生成 / 取消确认框 / 关闭补全菜单              |
| `Ctrl+C`                                 | 退出                                              |

**输入补全(实时)**:输入 `/` 弹出**命令菜单**(内置命令 + 已安装技能);输入 `@` 弹出**文件/目录补全**(可 `@dir/` 逐层深入、`@~/` 到家目录)。候选随输入实时过滤,第一个候选同时以淡色 **ghost** 显示在输入框内。菜单打开时:`↑`/`↓` 选择、**`Tab` 或 `Enter` 采纳**(采纳后需再按一次 `Enter` 才发送,避免误触技能/命令)、`Esc` 关闭。

### 2.2 鼠标

- 消息区内**滚轮**上下滚动;内容超出时消息区右侧出现**滚动条**,可点击任意位置跳转、按住拖动实时滚动(需终端支持鼠标事件;Windows 的 Windows Terminal / conhost 亦支持)。
- 启用鼠标后终端内直接拖选文本会失效,复制请按住 `Shift` 再拖动选择。

### 2.3 工具卡片与确认框

- 模型干活时的每一步都会以**工具卡片**实时显示:`⏳` 执行中 → `✓` 成功 / `✗` 失败(预览输出;命令/工具输出**保留 ANSI 颜色**,其余控制字节已净化、不会干扰画面)。模型自主调用技能时卡片显示 `skill「技能名」`。
- 需要写文件或执行命令时弹出**确认框**:`y` 允许一次 / `a` 本次会话都允许 / `n` 拒绝 / `Esc` 取消。

---

## 3. 配置向导(首次运行)

未配置(无 `~/.znaide/config.json`)时自动打开;任何时候输入 `/config` 重新打开,修改即时生效、不中断会话。步骤:

1. **选择服务商**:`↑/↓` 选择内置预设(ollama / dashscope / deepseek / openrouter / zai),或选 **custom** 手输任意 OpenAI 兼容端点。
2. **选择模型**:程序自动请求该服务商的 `/models` 接口拉取真实可用模型,`↑/↓` 选择;查询失败或想自定义时按 `m` 直接输入模型名。
3. **填 API Key**:按提示输入(本地服务如 ollama 可留空)。Key 只写入 `~/.znaide/config.json`,不会出现在日志。
4. **验证并解锁**:按提示发送一条真实请求验证;**验证通过才保存配置并允许使用**;失败会提示你修改。

---

## 4. 输入技巧

### 4.1 多行与粘贴

- `Shift+Enter` / `Alt+Enter` / `Ctrl+J` 换行(支持在输入框内多行编辑)。
- 从剪贴板粘贴会以完整文本进入输入框(**多行也不会被误发送**),回车才发送。

### 4.2 @引用文件 / 目录

在指令里写 `@路径`(相对或绝对路径),发送前自动展开:

- `@文件` → 注入该文件内容(上限 2MB / 前 800 行,更大请用工具读取);
- `@目录` → 注入该目录条目清单(前 200 项);
- **`@"带空格/标点路径"`** → 用引号包裹即可引用任意文件名(普通 `@` 以空白为界,含空格会被截断)。

输入 `@` 时会出现**路径补全**(见 2.1),逐层选择后自动转成正确的引用形式。

示例:`把 @README.md 里的安装步骤重写一遍`;`看 @"./docs/我的 笔记.txt"`

---

## 5. 权限与安全

### 5.1 四档权限模式(Shift+Tab 循环)

| 模式(界面显示)                 | 行为                                                         | 边框颜色 |
| ------------------------------ | ------------------------------------------------------------ | -------- |
| **询问** `ask`(默认)           | 写文件、执行命令前均弹确认框                                 | 绿       |
| **编辑放行** `acceptEdits`     | 文件修改自动放行,命令仍确认                                  | 天蓝     |
| **全自动** `bypassPermissions` | 全自动执行,危险命令仍被拦截                                  | 紫       |
| **超级** `yolo`                | 一切放行、无任何问询,危险命令黑名单也跳过(高度危险,后果自负) | 红       |

状态栏实时显示当前模式(中文短名),**输入框与输出框(消息区)边框颜色随模式**变化(输入框工作中禁用时变灰);也可在 CLI 用 `--permission ask|acceptEdits|bypassPermissions|yolo` 指定启动模式。

### 5.2 危险命令黑名单

`rm -rf /`、`mkfs`、dd 写盘、`git push --force` 等启发式识别,**默认拒绝**(唯一例外:超级 `yolo` 模式连黑名单也放行)。

### 5.3 undo 快照

每次 `write_file` / `edit` 修改文件前,原文件自动快照到 `~/.znaide/undo/`。用 `/undo` 查看、`/undo <序号>` 回滚,不依赖 git。

---

## 6. 命令参考

| 命令                       | 作用                                                         |
| -------------------------- | ------------------------------------------------------------ |
| `/help`                    | 帮助                                                         |
| `/skills`                  | 列出已安装技能(含扫描告警)                                   |
| `/config`                  | 打开配置面板,随时改服务商/模型/端点/Key                      |
| `/undo`                    | 列出 undo 快照(最新在前)                                     |
| `/undo <序号>`             | 回滚到指定快照                                               |
| `/resume`                  | 列出历史会话                                                 |
| `/resume <序号\|片段>`     | 恢复某历史会话(支持序号、文件名片段、状态栏里的会话 ID)      |
| `/resume del <序号\|片段>` | 删除某历史会话                                               |
| `/clear`                   | 清空当前会话上下文与历史文件,重新开始(弹窗确认,不可恢复)     |
| `/compact`                 | 压缩上下文:旧对话收敛为一段中文摘要,上下文回到低位(历史文件完整保留)。过程不可中断:状态栏「压缩中…」+ 收纳推进条动画,消息区忙行提示约需数秒~数十秒 |
| `/quit` `/exit`            | 退出程序(退出时打印本次会话统计;Ctrl+C 同效)                 |
| `/技能名 [参数]`           | 手动触发技能(见第 7 节)                                      |

> 提示:除界面 `/resume` 外,也可用 `znaide --resume <会话ID|片段>` 在**启动时**直接恢复指定会话(交互模式;ID 见底部状态栏或退出统计)。

---

## 7. 技能(Skills)——扩展能力的主要方式

**技能 = 一个能力包**:针对某类任务的"操作说明书"(SKILL.md),可选附一个**入口脚本**在调用时自动执行并把输出喂给模型。技能不新增权限面——技能内部所有写文件/命令动作仍走权限三档、黑名单与 undo。

### 7.1 安装与发现

把技能目录放进以下任一层(同名按 **项目 > 用户** 覆盖):

```
~/.znaide/skills/<技能名>/SKILL.md       # 用户级(推荐)
.znaide/skills/<技能名>/SKILL.md         # 项目级(放在当前工作目录,随仓库分发)
```

分发包附带 `skills示例/`,把它里面的子目录复制到 `~/.znaide/skills/` 即可,例如:

```
mkdir -p ~/.znaide/skills
cp -r skills示例/* ~/.znaide/skills/    # Linux/macOS
# Windows:把 skills示例 下的子目录复制到 %USERPROFILE%\.znaide\skills\
```

装好后输入 `/skills`,会列出每个技能的名字与描述(附来源与"仅手动"标记)。技能是**每回合动态扫描**的,无需重启。

### 7.2 两种触发方式

- **模型自主调用**:技能以单个 `skill` 工具暴露给模型(可用技能名在参数列表里)。你只需要说一句符合技能描述的话,如"把下载目录整理一下",模型判断匹配就会调用,并显示 `skill「技能名」` 卡片。
- **手动触发**:直接输入 `/技能名 [参数]`。

> 某些技能在 SKILL.md 里声明 `disable-model-invocation: true`,表示它只允许手动触发、不出现在模型可调用列表。

### 7.3 SKILL.md 格式

```markdown
---
description: 一句话说明这个技能做什么、什么时候用(给模型判断)
disable-model-invocation: true   # 可选:true = 只能手动 /名字 触发
entry: scripts/run.sh            # 可选:调用时自动执行一次,stdout 并入上下文
---
正文:教模型一步步怎么做。正文里的 {args} 会被替换为命令参数;
可指示模型先读同目录 scripts/ 下的附件(如检查清单)。
```

字段速查:

| frontmatter 字段           | 必填 | 说明                                                 |
| -------------------------- | ---- | ---------------------------------------------------- |
| `description`              | 推荐 | 出现在 /skills、模型判断依据                         |
| `name`                     | 否   | 若写须与目录名一致,否则该技能被跳过并告警            |
| `disable-model-invocation` | 否   | 默认 false                                           |
| `entry`                    | 否   | 入口脚本,须为技能目录内相对路径,不得含 `..`/绝对路径 |

### 7.4 入口脚本(跨平台)

- **macOS/Linux**:放 `.sh`,用 bash 执行(不依赖可执行位与 shebang);
- **Windows**:同一技能放同名 `.ps1`(或 `.bat`/`.cmd`),引擎自动按 `.ps1 → .bat → .cmd` 找;都找不到会提示并按正文执行。
- 调用参数 `{args}` 经命令行传入;环境变量 `SKILL_DIR` 指向技能目录,脚本可据此定位自身附件。
- 技能正文请写成跨平台指令(区分 `mv` ↔ `move`/`Move-Item` 等),不要只写自己机器的写法。
- 项目级(第三方仓库自带)技能的入口脚本执行前同样会走权限确认;项目内容的 SKILL.md 属于不可信输入,需警惕其正文夹带"越权/删改/外发"类指令。

### 7.5 常见排查

- `/skills` 列表为空 → 检查目录结构是否为 `skills/<名字>/SKILL.md`、目录名是否只含字母/数字/`_`/`-`(≤40 字符)、frontmatter 的 `name` 是否与目录名一致;
- 列表有 `⚠` 告警行 → 按提示修正(如 entry 越界、无 SKILL.md);
- 想调试 → 先 `/技能名` 手动触发,看入口输出与模型行为。

---

## 8. 内置工具清单

模型可用以下工具自主执行(你通常不需要手动调用,但知道边界有助提要求):

| 工具                    | 作用                             | 关键参数                                      |
| ----------------------- | -------------------------------- | --------------------------------------------- |
| `list_directory`        | 列出目录条目                     | `path`(缺省当前目录)                          |
| `read_file`             | 分页读文本文件                   | `path`、`offset`、`limit`                     |
| `write_file`            | 新建/整体覆写(写前自动 undo)     | `path`、`content`                             |
| `edit`                  | 精确替换文中一段(写前自动 undo)  | `path`、`old_string`、`new_string`            |
| `glob`                  | 按通配符找文件                   | `pattern`、`path`                             |
| `grep_search`           | 正则搜文件内容                   | `pattern`、`path`、`glob`                     |
| `run_shell_command`     | 执行终端命令                     | `command`、`cwd`、`timeout_ms`                |
| `memory_write`          | 写长期记忆                       | `name`、`content`、可选 `description`/`mtype` |
| `memory_read`           | 读长期记忆                       | `query` 或 `name`                             |
| `web_fetch`             | 抓网页转纯文本                   | `url`、`max_chars`                            |
| `skill`                 | 启用技能(动态)                   | `name`(enum)、`args`                          |
| `mcp__<server>__<tool>` | MCP 服务器提供的工具(配置后出现) | 视工具而定                                    |

---

## 9. 长期记忆

- 位置:`~/.znaide/memories/`,每条记忆一个 markdown 文件 + `MEMORY.md` 索引。
- 会话启动时自动把记忆索引摘要注入给模型;任务中模型可随时 `memory_read` / `memory_write`。
- 适合记录:你的机器环境、常用路径、操作偏好、进行中的项目背景。

---

## 10. MCP

编辑 `~/.znaide/mcp.json`,启动后自动连接并把工具并入工具箱(命名 `mcp__<server>__<tool>`):

```json
{
  "mcpServers": {
    "filesystem": { "command": "node", "args": ["/path/to/server.js"] }
  }
}
```

MCP 工具按"写操作"对待,`ask` 模式下先征求确认。

---

## 11. 会话与历史

- 每轮消息追加写入 `~/.znaide/sessions/<会话id>.jsonl`(仅用户/助手/工具消息,不含系统提示)。
- 列表会跳过 0 字节文件(还没对话的会话)。
- `/resume` 恢复后,历史内容会完整显示在消息区(含工具调用与结果),可直接继续对话;当前上下文被替换为该历史。
- 多实例互不干扰:可用 `ZNAIDE_DATA_DIR` 指向别的数据目录。

---

## 12. 无头模式(脚本 / CI)

```bash
# 单次执行,默认 ask:写文件/命令会被拒绝,按需放开
znaide -p "看看这个目录里有什么"

# 允许自动改文件(命令仍需确认)
znaide -p "把 README 里的 X 改成 Y" --permission acceptEdits

# 全自动执行
znaide -p "把 ~/Downloads 里的 zip 按日期归档" --permission bypassPermissions

# 临时指定模型/服务商(不改配置文件)
znaide -p "你好" --provider deepseek
znaide -p "你好" --model qwen3:8b --base-url http://localhost:11434/v1
```

CLI 参数:

| 参数                                                 | 说明                   |
| ---------------------------------------------------- | ---------------------- |
| `-p, --print <PROMPT>`                               | 无头模式执行单条指令   |
| `--permission <ask\|acceptEdits\|bypassPermissions>` | 启动权限模式(默认 ask) |
| `--cwd <DIR>`                                        | 工作目录(默认当前目录) |
| `--provider <NAME>`                                  | 服务商预设名           |
| `--model <MODEL>`                                    | 覆盖模型名             |
| `--base-url <URL>`                                   | 覆盖 OpenAI 兼容端点   |
| `--api-key <KEY>`                                    | 覆盖 API Key           |
| `--version` / `--help`                               | 版本 / 帮助            |

---

## 13. 配置

### 13.1 配置文件 `~/.znaide/config.json`

```json
{
  "provider": "ollama",
  "model": "qwen3:8b",
  "base_url": "http://localhost:11434/v1",
  "api_key": "",
  "context_window": 40960,
  "providers": {
    "ollama": { "base_url": "http://localhost:11434/v1", "model": "qwen3:8b" },
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

字段:`provider` 当前服务商;顶层 `model`/`base_url`/`api_key` 覆盖预设;`providers` 预设表(可自定义服务商)。`api_key_env` 表示从该环境变量读 Key(推荐,避免明文)。

`context_window`(可选):当前模型的上下文窗口 token 数,状态栏的 ctx 占用条按它计算。不设置时按模型名匹配内置表(2026-09 检索:qwen3→40k、qwen-plus→1M、deepseek-v4→1M、gpt-5→400k、claude→200k 等),未命中默认 32k。本地模型(ollama)请与运行时的 `num_ctx` 保持一致,否则 ctx 占用条不准。

### 13.2 环境变量(优先级:命令行 > 环境变量 > config.json > 内置预设)

| 变量                                                         | 作用                      |
| ------------------------------------------------------------ | ------------------------- |
| `ZNAIDE_PROVIDER`                                            | 服务商预设                |
| `ZNAIDE_MODEL` / `OPENAI_MODEL`                              | 模型名                    |
| `ZNAIDE_BASE_URL` / `OPENAI_BASE_URL`                        | OpenAI 兼容端点           |
| `ZNAIDE_API_KEY` / `OPENAI_API_KEY`                          | API Key                   |
| `DASHSCOPE_API_KEY` / `DEEPSEEK_API_KEY` / `ZAI_API_KEY` / `OPENROUTER_API_KEY` | 各预设的 Key              |
| `ZNAIDE_DATA_DIR`                                            | 覆盖数据目录(测试/多实例) |

---

## 14. 数据目录结构

```
~/.znaide/
├─ config.json      配置(provider / model / base_url / api_key)
├─ mcp.json         MCP server 配置(可选)
├─ skills/          技能(每个子目录一个技能)
├─ sessions/        会话历史 *.jsonl
├─ memories/        长期记忆(*.md + MEMORY.md 索引)
└─ undo/            写文件前快照(manifest.jsonl + files/)
```

- 旧版 `commands/*.md`(如从老版本升级)会在启动时自动迁移为 `skills/<名字>/SKILL.md`,保持仅手动触发。
- 备份:复制 `~/.znaide` 即可;恢复时放回原位置。

---

## 15. 常见问题

**Q:启动一直停在"未配置"?**
运行配置向导完成验证;或手动写好 `~/.znaide/config.json`,或命令行指定 `--model --base-url`。

**Q:模型回"工具参数解析错误"?**
多为模型本身输出损坏的 JSON(小模型常见)。程序会给出原文提示,模型会自动重试;也可换更强/更高量化档的模型。命令类参数请尽量让模型保持简单写法。

**Q:技能没有出现在 /skills?**
见 7.5 排查。重点:目录结构、目录名合法、frontmatter `name` 与目录一致、SKILL.md 可读。

**Q:无法用鼠标选择复制文字?**
启用鼠标捕获后需按住 `Shift` 再拖动选择(见 2.2)。

**Q:Windows 下技能入口脚本不跑?**
确认技能目录里有与 `.sh` 同名的 `.ps1`(或 `.bat/.cmd`);若提示"当前平台没有可用入口脚本",按正文仍可执行。

**Q:undo 能回滚删除/命令吗?**
undo 只覆盖 `write_file`/`edit` 的写前快照;`rm`/`mv` 等命令操作不在 undo 范围,重要操作前请谨慎或要求模型先备份。

---

## 16. 附录:速查

**键盘**:Enter 发送 · Shift+Enter/Alt+Enter/Ctrl+J 换行 · Shift+Tab 切权限 · ↑/↓、PgUp/PgDn、Home/End 滚动 · Esc 中断/取消 · Ctrl+C 退出
**确认框**:y 一次 · a 本会话放行 · n 拒绝 · Esc 取消
**命令**:/help · /skills · /config · /undo [序号] · /resume [片段] [del] · /clear · /quit(/exit) · /技能名
**目录**:配置 `~/.znaide` · 技能 `~/.znaide/skills` 或 `.znaide/skills` · 会话 `~/.znaide/sessions` · 记忆 `~/.znaide/memories` · 快照 `~/.znaide/undo`
