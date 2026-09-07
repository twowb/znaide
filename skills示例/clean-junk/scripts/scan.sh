#!/usr/bin/env bash
# 只读垃圾/缓存扫描(macOS/Linux),输出 CAND|标签|大小|路径,不做任何删除。
set -u

H="$HOME"
OS="$(uname -s 2>/dev/null || echo Linux)"

case "$OS" in
  Darwin)
    TRASH="$H/.Trash"
    CACHEDIR="$H/Library/Caches"
    PIP="$CACHEDIR/pip"
    APT=""   # macOS 无 apt;Homebrew 缓存在 CACHEDIR 顶层子目录里可见
    ;;
  *)
    TRASH="$H/.local/share/Trash"
    CACHEDIR="$H/.cache"
    PIP="$CACHEDIR/pip"
    APT="/var/cache/apt/archives"
    ;;
esac

size_of() { # size_of <路径>
  local p="$1"
  if [ -e "$p" ]; then du -sh "$p" 2>/dev/null | awk '{print $1}'; else echo "(无)"; fi
}

echo "== 候选清理项(只读扫描,不会删除任何东西)=="

echo "CAND|回收站|$(size_of "$TRASH")|$TRASH"
echo "CAND|用户缓存|$(size_of "$CACHEDIR")|$CACHEDIR"

# 缓存目录里最大的几个子目录(便于只清大头)
top_cache=$(timeout 25 bash -c 'du -shx "$0"/* 2>/dev/null | sort -rh | head -6' "$CACHEDIR" 2>/dev/null)
if [ -n "$top_cache" ]; then
  echo "$top_cache" | while read -r size path; do
    echo "CAND|缓存内: $(basename "$path")|$size|$path"
  done
fi

echo "CAND|pip 缓存|$(size_of "$PIP")|$PIP"
echo "CAND|npm 缓存|$(size_of "$H/.npm/_cacache")|$H/.npm/_cacache"
echo "CAND|cargo registry 缓存|$(size_of "$H/.cargo/registry/cache")|$H/.cargo/registry/cache"
echo "CAND|gradle 缓存|$(size_of "$H/.gradle/caches")|$H/.gradle/caches"
echo "CAND|Maven 仓库缓存|$(size_of "$H/.m2/repository")|$H/.m2/repository"
if [ -n "$APT" ]; then
  echo "CAND|apt 包缓存|$(size_of "$APT")|$APT"
fi

# /tmp 里超过 7 天的普通文件(仅统计,清理需用户点名)
old_tmp=$(find /tmp -maxdepth 1 -type f -mtime +7 -printf '%s\n' 2>/dev/null | awk '{s+=$1} END {if (s>0) printf "%.1fM", s/1048576; else print "(无)"}')
old_cnt=$(find /tmp -maxdepth 1 -type f -mtime +7 2>/dev/null | wc -l)
echo "CAND|/tmp 旧文件(>7 天,$old_cnt 个)|$old_tmp|/tmp"

echo "== 扫描完成:请把 CAND 行整理成编号清单,等用户挑选后再执行 =="
