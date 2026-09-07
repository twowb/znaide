# znaide

**A "do-anything" AI assistant that lives in your terminal.**

Tell it what to do in plain language and it gets it done — edit files, run commands, browse the web, organize folders, write documents. Coding is just one use case. Written from scratch in Rust (agent-loop architecture inspired by qwen-code): **one static binary, zero Node/zero runtime dependencies, Chinese-first UI, works with local models and cloud APIs alike**.

> **Source & binaries**: This tool is open source under **AGPL-3.0** (repo: https://github.com/twowb/znaide). Free for personal use, learning, and open-source derivatives; **commercial or closed-source integration requires a commercial license** — see `COMMERCIAL.md`. Don't want to build? Grab a binary from [GitHub Releases](https://github.com/twowb/znaide/releases): pick `znaide-<platform>-v<version>` for Linux/macOS/Windows/Android-Termux; `--version` shows the build.

```
  znaide                                  # interactive mode (first run opens the setup wizard)
  znaide -p "archive zips in ~/Downloads by date"   # headless: run one task
```

## Features

- **General-purpose agent**: built-in tools + agent loop, autonomously "investigate → act → report" — not limited to coding
- **Any OpenAI-compatible backend**: local ollama / vLLM, cloud DeepSeek / DashScope (Qwen) / OpenRouter, etc. — any `base_url + key + model`
- **Setup wizard**: step-by-step on first run — pick a provider → **auto-fetch its real model list** (`/models`) → choose/type a model → enter key → **unlocks only after a live request succeeds**; reopen anytime with `/config`, changes apply immediately
- **Toolbox**: file read/write/edit (auto-snapshot before writes), directory listing, glob, regex search, shell commands (with timeouts), web fetch, long-term memory
- **Four permission tiers**: **ask** (default; confirm file writes/commands) / **acceptEdits** (file edits auto-approved) / **bypassPermissions** (fully automatic) / **yolo** (everything allowed, dangerous commands included — on you); cycle with `Shift+Tab`. Dangerous commands (`rm -rf /`, `dd` to disk, `git push --force`, …) are blocked by default; only **yolo** skips the blacklist
- **undo**: auto-snapshot before every file change; `/undo` rolls back — no git needed
- **Long-term memory**: remembers your environment and preferences across sessions; injected into context at startup
- **MCP support**: drop a `~/.znaide/mcp.json` and any MCP server's tools join the toolbox
- **Session history**: every turn persisted as JSONL; `/resume` picks up where you left off
- **Skills**: capability packs (a Markdown manual + optional entry script) the model can call on its own or you can trigger with `/skill-name`; hot-pluggable across three layers
- **`@file` references**: type `@path` and the file contents / directory listing are injected for the model; `@"paths with spaces"` works too; path completion while typing (see below)

## Quick start

### Option 1: build it (needs Rust toolchain)

```bash
cargo build --release
# binary: target/release/znaide
```

### Option 2: `make` cross-platform build (recommended)

```bash
make build               # native platform (auto-detected)
make build-linux         # Linux x86_64 (fully static musl)
make build-linux-arm64   # Linux aarch64 (fully static musl)
make build-windows       # Windows x86_64 (static CRT)
make build-macos         # macOS x86_64 (zig cross-compile)
make build-macos-arm64   # macOS aarch64 (zig cross-compile)
make build-android       # Android aarch64 (NDK bionic, for Termux; override NDK_ROOT)
make build-all           # all six platforms (skips missing toolchains; android needs NDK)
```

All builds are `--release` minimal (opt-level=z + LTO + stripped symbols), output in `dist/`. Cross-build details: [Makefile](./Makefile).

> First cross-compile may ask you to `rustup target add <triple>`; Linux musl static needs musl cross-gcc (Ubuntu: `sudo apt install musl-tools`); macOS uses **zig universal cross-compilation** (no osxcross/SDK), needing the `zig-cc-*-darwin` wrappers in `~/.cargo/bin` plus `~/.cargo/darwin-stubs/libiconv.tbd` (see Makefile and `.cargo/config.toml`).

## First-run setup wizard

On first launch (no `~/.znaide/config.json` yet), interactive mode opens the wizard:

1. **Pick a provider**: built-in ollama / dashscope / deepseek / openrouter / zai, or custom (type any endpoint)
2. **Pick a model**: the app fetches the provider's **real model list** from `/models`; `↑↓` to choose; press `m` to type one manually
3. **Enter API key**: press `e` to type it (local servers like ollama can leave it empty)
4. **Verify & unlock**: press `s` to send a real test request — **the config is only saved once the request succeeds**; on failure it tells you what to fix

Reopen anytime with `/config` to switch provider/model/key; changes take effect immediately (no session restart).

## Usage

### Interactive mode

```
  znaide
  znaide --resume <sessionID|fragment>   # resume a history session at startup (ID in the status bar / exit stats)
```

A banner shows at startup; `/quit` or `/exit` exits (or `Ctrl+C`), printing this session's stats in the normal terminal (duration / messages / tool calls / tokens / undo snapshots / session ID). Common keys:

| Key | Action |
|---|---|
| `Enter` | Send |
| `Shift+Enter` (or `Alt+Enter` / `Ctrl+J`) | New line |
| `Shift+Tab` | Cycle permission mode (ask → acceptEdits → bypassPermissions → yolo) |
| `↑` / `↓` | Scroll message area 3 lines |
| `PageUp` / `PageDown` | Scroll message area 15 lines |
| `Home` / `End` | Jump to top / bottom |
| `Esc` | Interrupt generation / cancel confirm |
| `Ctrl+C` | Quit |

**Live completion**: typing `/` opens a command menu (built-ins + installed skills); typing `@` opens file/dir completion (`@dir/` digs deeper, `@~/` goes home) — candidates filter as you type, first candidate previewed as a dim **ghost** in the input. `↑`/`↓` to pick, **`Tab` or `Enter` to accept** (press `Enter` again to actually send — prevents misfires), `Esc` to close. Names with spaces/punctuation are auto-wrapped into `@"path with spaces"` form.

**Mouse**: wheel scrolls; a scrollbar appears on the right when content overflows — click to jump or drag to scroll (needs terminal mouse support; Windows Terminal / conhost work too). To copy text, hold `Shift` and drag-select.

**Tool cards** show what the AI is doing in real time: the exact command under execution (`$ ls -la`), elapsed seconds and the AI's own time budget; a command that stalls past its budget is terminated and the partial output is fed back to the AI so it can retry with a bigger budget, split the work, or switch approach. A running command also gets a **live output pane** under the card — like `tail -f`, showing the latest ~10 lines with ANSI colors, auto-following the bottom (pausing while you scroll up). Cards show `⏳ → ✓/✗` (pulsing animation while running), and keep a dim copy of the call afterward. Confirm prompts: `y` once / `a` always this session / `n` deny; destructive ops (e.g. `/clear`) get their own confirm: `y` / `n` or `Esc`.

**Dynamic timing on cards**: `⏳ run_shell_command · 12s · budget 4min` ticking per second; total time shown when done. Pass `idle_ms` as your **estimated completion time** (better to overshoot; unset = no budget): if the command overruns its estimate (plus a grace of ~20%, capped at 60s) it is terminated with a full report; commands silent for 90s are considered hung; cards turn yellow → red near the limits, and show "over budget, grace left Xs" while in the grace period.

**Status bar** (left → right): `permission (询问/编辑放行/全自动/超级) [| persona (when set)] | model | state (ready/working…/compacting…, with flow/compact animations) [| tokens: in · out · Σ (≈ live while streaming)] [| ctx ▓▓▓▓▓░░░░░ 34% usage bar (vs model window; 70% yellow / 90% red hinting /compact)] · session <id>`.

Input/output box border colors follow the permission mode (ask=green / acceptEdits=cyan / bypass=magenta / yolo=red). Session ID matches its history file (`~/.znaide/sessions/<id>.jsonl`) and works with `/resume`.

### Slash commands

| Command | Action |
|---|---|
| `/help` | Show help |
| `/skills` | List installed skills (with scan warnings) |
| `/config` | Open config panel (provider/model/endpoint/key, applies instantly) |
| `/undo` `/undo <n>` | List snapshots / roll back to one |
| `/resume` `/resume <n\|fragment>` | **No arg**: opens the full-screen **session manager** ("sessions / memory" tabs: `↑↓` pick, `space` multi-select, `d` batch delete, `n` edit note, `/` filter, Enter resume/view; the current session can't be deleted). **With arg**: resume that session directly (`[headless]` = created by `-p`) |
| `/clear` | Clear session context + history file (confirmed; unrecoverable) |
| `/compact` | Compress context: older turns collapsed into a summary, context freed (full history kept for `--resume`) |
| `/update` | Check & update to the latest release (`znaide --update` also works; startup auto-checks once). **Two sources with automatic fallback**: **Gitee mirror** first (synced repo & releases, no proxy needed in China); when unreachable (or download fails) it switches to GitHub; `HTTPS_PROXY` still helps the GitHub source. Linux/macOS replace on next start, Windows swaps after exit |
| `/persona` | Global persona: list / switch (`/persona <name>`), `/persona none` off. Persisted to config, affects all following conversation; safety rules stay intact |
| `/quit` `/exit` | Quit (prints session stats; `Ctrl+C` too) |
| `/skill-name [args]` | Trigger an installed skill manually (see Skills) |

### Personas

A global personality for the AI. `/persona` lists/switches built-ins: **Sarcastic Buddy / Patient Teacher / Hype Geek / Minimal & Cold**; `/persona none` back to default. The switch is persisted and shapes every later reply (status bar shows who's talking). Personas only affect tone and perspective — permissions, dangerous-command blocking and tool discipline are never overridden.

**A persona is just a Markdown file.** The four built-in personas ship as files under `~/.znaide/personas/` — open one, tweak the wording, restart, done; delete a file to restore the built-in default; add any `<name>.md` to create your own; share one by copying the file.

### Skills

**A skill = a capability pack the model can invoke on its own, or you can run with `/name`.** The model calls a `skill` tool with the matching name; the manual body (with `{args}` rendered) plus any entry-script output is injected, and the model follows the manual using base tools. Skills add no privileges — every write/command still goes through permission tiers, the dangerous-command blacklist and undo snapshots.

Three layers (same name → **project overrides user, user overrides built-in**):

```
built-in (out of the box)          # archive-downloads / clean-junk / weekly-report
~/.znaide/skills/<name>/SKILL.md    # user level (recommended)
.znaide/skills/<name>/SKILL.md      # project level (ships with a repo; treat content as untrusted)
```

The three sample skills (archive downloads / junk scanner / weekly report) are **compiled into the binary** — nothing to install; hydrated to `~/.znaide/builtin-skills/` where you can read or copy them. Releases no longer ship a separate skills pack.

SKILL.md format (minimal frontmatter):

```markdown
---
description: Archive zips in ~/Downloads by date (tells the model when to use it)
disable-model-invocation: true   # optional: true = manual /name only, not shown to the model
entry: scripts/run.sh            # optional: auto-run once on invoke, stdout joins context
---
Body: teach the model what to do. `{args}` is replaced by the arguments.
```

Entry scripts are **cross-platform**: macOS/Linux use `.sh` (run via bash, no +x needed); on Windows the engine looks for the same-name `.ps1` → `.bat` → `.cmd`. `SKILL_DIR` env var points at the skill directory. Write platform-aware instructions (`mv` vs Windows `move`/`Move-Item`).

> Legacy `~/.znaide/commands/*.md` migrate automatically at startup.

### Headless mode (scripts / CI)

```bash
# single run; ask mode by default (writes/commands denied — loosen as needed)
znaide -p "see what's in this directory"

# allow automatic file edits (commands still confirmed)
znaide -p "change X to Y in README" --permission acceptEdits

# fully automatic
znaide -p "archive zips in ~/Downloads by date" --permission bypassPermissions

# override model/provider without touching config
znaide -p "hi" --provider deepseek
znaide -p "hi" --model qwen3:8b --base-url http://localhost:11434/v1
```

## Configuration

### `~/.znaide/config.json`

```json
{
  "provider": "ollama",
  "model": "qwen3:8b",
  "base_url": "http://localhost:11434/v1",
  "api_key": "",
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

- `provider`: active preset (must exist in `providers` or be a built-in)
- Top-level `model` / `base_url` / `api_key` override the preset
- `api_key_env`: read the key from that environment variable (recommended, keeps secrets out of the file); a plain `api_key` also works
- `context_window` (optional): the model's context window in tokens, used by the ctx usage bar. When unset, an internal table matches the model name (2026-09 data: qwen3→40k, qwen-plus→1M, deepseek-v4→1M, gpt-5→400k, claude→200k…), falling back to 32k; for ollama keep it in sync with `num_ctx`

Keys can live purely in environment variables: `DASHSCOPE_API_KEY`, `DEEPSEEK_API_KEY`, `ZAI_API_KEY`, `OPENROUTER_API_KEY`, etc.

### Environment variables

Precedence: **CLI args > env vars > config.json > built-in presets**.

| Variable | Purpose |
|---|---|
| `ZNAIDE_PROVIDER` | Provider preset |
| `ZNAIDE_MODEL` / `OPENAI_MODEL` | Model name |
| `ZNAIDE_BASE_URL` / `OPENAI_BASE_URL` | OpenAI-compatible endpoint |
| `ZNAIDE_API_KEY` / `OPENAI_API_KEY` | API key |
| `ZNAIDE_DATA_DIR` | Override data dir (default `~/.znaide`; testing/multi-instance) |

### Data layout `~/.znaide/`

```
~/.znaide/
├─ config.json      config (provider / model / base_url / api_key / persona)
├─ mcp.json         MCP servers (optional)
├─ skills/          user skills (skills/<name>/SKILL.md, may carry scripts/)
├─ builtin-skills/  hydrated built-in samples (archive-downloads / clean-junk / weekly-report)
├─ personas/        persona files (<name>.md, body = persona description)
├─ sessions/        session history *.jsonl (may carry a sibling *.meta.json note)
├─ memories/        long-term memory (*.md + MEMORY.md index)
└─ undo/            pre-write snapshots (manifest.jsonl + files/)
```

## MCP

Edit `~/.znaide/mcp.json`:

```json
{
  "mcpServers": {
    "filesystem": { "command": "node", "args": ["/path/to/server.js"] }
  }
}
```

On startup znaide connects and merges MCP tools into the toolbox as `mcp__<server>__<tool>`; MCP tools count as write operations and ask for confirmation in **ask** mode.

## Project layout

```
znaide/
├─ crates/
│  ├─ core/     engine: agent loop, LLM client (OpenAI-compatible), tools, permissions, skills, memory, sessions, undo, MCP, config (pure logic, no terminal deps)
│  ├─ tui/      interface (ratatui): conversation, input, wizard, confirm, tool cards, scrollbar & mouse
│  └─ cli/      entry: interactive / -p headless / args
├─ docs/        user manual (使用说明.md)
└─ Makefile     cross-platform builds
```

## Development

```bash
cargo build              # debug build
cargo test               # unit + integration tests (local mock e2e)
cargo test -p znaide-tui # TUI tests
```

## License

AGPL-3.0 — see [LICENSE](./LICENSE). Commercial or closed-source use requires a [commercial license](mailto:zngeek@pm.me) (see [COMMERCIAL.md](./COMMERCIAL.md)).
