#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# 备份：打包（含 .git 历史）→ 放 WSL ext4 → 再镜像一份到 Windows C 盘。
# 目标盘都在不同物理盘上：X:（源码）、/dev/sdd（WSL）、C:（系统盘）。
set -euo pipefail
SRC="$UI_ROOT"
DEST=${DEST:-$HOME/backups}
MIRROR=${MIRROR:-/mnt/c/Users/31440/backups}
KEEP=${KEEP:-7}
STAMP=$(date +%Y%m%d-%H%M%S)
mkdir -p "$DEST"

OUT="$DEST/harness-v2-$STAMP.tar.gz"
tar czf "$OUT" -C /mnt/x/dsh_ws \
  --exclude='harness-v2/out' \
  --exclude='harness-v2/target' \
  --exclude='harness-v2/.ui' \
  harness-v2

SIZE=$(du -h "$OUT" | awk '{print $1}')
COUNT=$(tar tzf "$OUT" | wc -l)
echo "备份: $OUT"
echo "体积 $SIZE · 条目 $COUNT"

if tar tzf "$OUT" >/dev/null 2>&1; then
  echo "校验: 归档完整"
else
  echo "校验: 归档损坏"
  exit 1
fi

# 镜像到 Windows 盘
if mkdir -p "$MIRROR" 2>/dev/null; then
  if cp "$OUT" "$MIRROR/" 2>/dev/null; then
    echo "镜像: $MIRROR/$(basename "$OUT")"
  else
    echo "镜像失败（跳过）：$MIRROR"
  fi
fi

# 两边各自保留最近 KEEP 份
for dir in "$DEST" "$MIRROR"; do
  [ -d "$dir" ] || continue
  ls -1t "$dir"/harness-v2-*.tar.gz 2>/dev/null | tail -n +$((KEEP + 1)) | while read -r old; do
    rm -f "$old"
    echo "清理: $(dirname "$old")/$(basename "$old")"
  done
done

echo "== 现有备份 =="
ls -lh "$DEST"/harness-v2-*.tar.gz 2>/dev/null | awk '{print $5, $9}'
ls -lh "$MIRROR"/harness-v2-*.tar.gz 2>/dev/null | awk '{print $5, $9}'
