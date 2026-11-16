#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# 把 X:\dsh_ws\harness-v2 同步到 WSL 的 ~/ui 构建副本。
# 源码在 ext4 上，cargo 的扫描/增量都快得多（实测：干净 19.6s→3.1s，增量 3.4s→0.04s）。
set -uo pipefail

SRC="$UI_ROOT"
DST="$UI_BUILD_DIR"

mkdir -p "$DST"

if command -v rsync >/dev/null 2>&1; then
  rsync -a --delete \
    --exclude target --exclude '.git' --exclude out \
    --exclude 'demo/.ui' --exclude 'demo/.loom' --exclude secrets \
    "$SRC"/ "$DST"/
else
  # 没有 rsync 就退化成 cp（不删除多余文件，够用）
  cp -a "$SRC"/. "$DST"/
  rm -rf "$DST/target" "$DST/out" "$DST/demo/.ui" "$DST/demo/.loom" "$DST/secrets"
fi

echo "synced → $DST"
