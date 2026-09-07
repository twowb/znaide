---
description: 检索当前系统的垃圾/缓存并估算大小,列清单让你挑选要清理的项目,选完才执行(跨平台 Linux/macOS/Windows)
entry: scripts/scan.sh
---
# 系统垃圾检索与清理(跨平台)

入口扫描脚本已按当前系统自动选择:Windows 用 `scripts/scan.ps1`,macOS/Linux 用 `scripts/scan.sh`(macOS 会自动切到 `~/Library/Caches` 视角)。输出里逐条 `CAND|标签|大小|路径`,**什么都没删**。

## 铁律:先挑后清,绝不擅自动手

1. 把扫描输出的每条 `CAND` 行整理成**编号清单**给用户,标注可释放大小与含义,**只介绍当前平台的项目**,不要混讲另一套路径。
2. **等待用户明确选择**(回复编号/名称均可);用户没说清就绝不清任何东西;用户说"全部清理"才允许全选。
3. 清理命令必须匹配当前平台:
   - **Linux/macOS**:`rm -rf` 精确路径;macOS 的 ~/Library/Caches 同理;回收站项按入口给出的路径清理;
   - **Windows**:通过 `run_shell_command` 执行 `powershell -NoProfile -Command "Remove-Item -LiteralPath '路径' -Recurse -Force"`;回收站项用 `Clear-RecycleBin -Force`(仅用户点名时);`%TEMP%` 旧文件只清 `LastWriteTime` 超过 7 天的。
4. 只处理用户选中的条目。**绝不**触碰:清单之外的任何文件、系统目录(Linux/macOS `/usr /var/lib /etc`;Windows `C:\Windows` 等)、用户文档/项目。
5. `/tmp`(Windows `%TEMP%`)只清超过 7 天的旧文件,且必须用户点名才动。
6. 无写权限的项(如 Linux apt 缓存需 root)如实说明并跳过,不要 sudo 或绕过。
7. 每项前后对比大小(`du -sh` / PowerShell 统计),最后给用户一张表:**清了哪些 → 释放了多少**。缓存被清后对应程序下次启动会自动重建,运行中的程序建议先退出。
