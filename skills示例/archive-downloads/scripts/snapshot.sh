#!/usr/bin/env bash
# 只读快照:列出来源目录的文件概况(macOS/Linux),不做任何改动。
# 用法:snapshot.sh [目录]  缺省 ~/Downloads
set -u
SRC="${1:-$HOME/Downloads}"

if [ ! -d "$SRC" ]; then
  echo "目录不存在: $SRC"
  exit 0
fi

echo "== 来源目录: $SRC =="
echo "== 最近下载(按时间倒序,最多 30 条) =="
find "$SRC" -maxdepth 1 -type f -printf '%TY-%Tm-%Td %TH:%TM %10s %f\n' 2>/dev/null | sort -r | head -30

echo "== 压缩包统计 =="
N=$(find "$SRC" -maxdepth 1 -type f \( -iname '*.zip' -o -iname '*.tar' -o -iname '*.tgz' \
    -o -iname '*.tar.gz' -o -iname '*.tar.bz2' -o -iname '*.7z' -o -iname '*.rar' \) 2>/dev/null | wc -l)
echo "压缩包: $N 个"
du -sh "$SRC" 2>/dev/null || true
