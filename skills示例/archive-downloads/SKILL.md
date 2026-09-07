---
description: 把系统下载目录(默认 ~/Downloads,参数可指定)里积压的压缩包按类型归档,并汇报结果(跨平台)
entry: scripts/snapshot.sh
---
# 归档下载目录(跨平台)

入口脚本已按当前平台自动选择快照版本:Windows 用 `scripts/snapshot.ps1`,macOS/Linux 用 `scripts/snapshot.sh`;输出即为当前平台的实际目录概况。

## 步骤

1. **确定来源目录**:默认 Linux/macOS 为 `~/Downloads`,Windows 为 `%USERPROFILE%\Downloads`;`{args}` 指定了目录则用它。目录不存在或为空 → 直接说明并结束,不要自行创建。
2. **复核压缩包清单**(只处理:`.zip` `.tar` `.tgz` `.tar.gz` `.tar.bz2` `.7z` `.rar`,不碰其它文件):
   - Linux/macOS:用 `list_directory` / `glob`;
   - Windows:用 `run_shell_command` 执行 `dir` 或 PowerShell `Get-ChildItem`。
3. **归类移动**到归档根(Linux/macOS `~/archives`,Windows `%USERPROFILE%\archives`),按文件名关键词分 `media`(音视频/图片)/ `documents`(文档/书籍)/ `software`(安装包)/ `misc`(其余)子目录。
4. **命令必须匹配当前平台**:
   - Linux/macOS:`mkdir -p` 建目录、`mv` 移动、`du` 看大小;
   - Windows:`mkdir` 或 PowerShell `New-Item -ItemType Directory -Force`;移动用 `move` 或 PowerShell `Move-Item`;大小用 PowerShell;
   - 含空格的路径一律加引号;Windows 上的复杂操作通过 `run_shell_command` 执行 `powershell -NoProfile -Command "…"`,不要用 Linux 语法硬套。
5. 冲突规则:目标同名时新名字追加 `-1`/`-2`,**绝不覆盖**;只归档,不删除、不解压。
6. 全部移动后,再列一次两个目录确认,给用户清单:**移了哪些 → 到了哪里**。每次移动命令都会弹出权限确认,逐条执行,不要拼成"一条命令干完"。
