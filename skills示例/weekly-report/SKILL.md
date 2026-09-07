---
description: 根据 git 提交历史生成一段中文周报(近 7 天;参数可指定仓库路径或天数,如 /weekly-report C:\work\myproj 3)
---
# 生成中文周报(跨平台)

1. **确定仓库**:`{args}` 里的目录若存在则进入它(Windows 用 `C:\...` 也行),否则用当前目录。
2. **采集数据**(用 `run_shell_command`,天数默认 7;`{args}` 出现"n天/N days"之类的数字则按它来):
   - `git log --since="<N> days ago" --pretty=format:"%h|%an|%ad|%s" --date=short`
   - `git log --since="<N> days ago" --stat --oneline`(看增删概况)
   - `git shortlog -sn --since="<N> days ago"`
   - **注意引号与平台**:Linux/macOS 的 shell 用单引号包格式串;Windows 的 cmd 用双引号并把 `|` 换成其它分隔符(如 `%h -- %an -- %s`)避免被当管道符,或改用 `powershell -NoProfile -Command "git log …"`。命令构造以当前系统为准,不要照抄另一平台写法。
3. **按以下结构输出中文周报**(纯文本即可,不写文件):
   - **本周概览**:提交数、涉及文件数、增删行数(有数据才写)、主要作者
   - **重点进展**:按提交信息归纳 3~6 条"做了什么",每条一句话并标注涉及模块
   - **风险与遗留**:半成品、未合并分支、明显的 TODO(仅在提交里看得到时写)
   - **下一步建议**:1~3 条,基于提交内容推断,标注为建议
4. 规矩:**不编造提交**,一切以 `git log` 实际输出为准;目录不是 git 仓库或没有提交时如实说明,不要硬造。
