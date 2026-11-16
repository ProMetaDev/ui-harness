#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# 真模型 + 中途取消：启动一轮，2 秒后按 Esc，看是否真的停下来。
set -uo pipefail
cd "$UI_ROOT"
export CARGO_TARGET_DIR="$UI_TARGET_DIR"
export UI_MODEL_CMD="bash tools/_node.sh tools/deepseek.mjs"
BIN="$CARGO_TARGET_DIR/release/ui"

cargo build --release --offline 2>&1 | tail -1
rm -rf demo/.ui demo/.loom

( printf '把你的计划详细写出来：先读哪些文件、按什么顺序改；先别动手\r'
  sleep 2
  printf '\x1b'          # 运行中 → 取消
  sleep 3
  printf '\x1b'          # 空闲 → 退出
  sleep 2
  printf '\x1b'          # 兜底
) | timeout 90 script -qec "$BIN tui --workspace demo --yes" /dev/null > /tmp/cancel-real.out 2>&1

echo "exit=$?  bytes=$(wc -c < /tmp/cancel-real.out)"
echo
echo "=== 日志尾部 ==="
"$BIN" replay --workspace demo | tail -6
