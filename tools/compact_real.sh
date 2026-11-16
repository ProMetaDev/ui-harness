#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# 真模型 + 激进折叠（800 tok）：验证「说过的事实被折叠后还捞得回来」。
set -uo pipefail
cd "$UI_ROOT"
export CARGO_TARGET_DIR="$UI_TARGET_DIR"
export UI_MODEL_CMD="bash tools/_node.sh tools/deepseek.mjs"
BIN="$CARGO_TARGET_DIR/release/ui"

cargo build --release --offline 2>&1 | tail -1
rm -rf demo/.ui demo/.loom

printf '你好，先别动手，我要交代几件事\r请记住：部署分支是 release/2026，别记错\rdemo 目录里有哪些文件\r我刚才让你记住哪个分支？复述一遍\r\x1b' \
  | timeout 300 script -qec \
      "$BIN tui --workspace demo --yes --max-calls 12 --compact-tokens 200 --keep-recent 4" \
      /dev/null > /tmp/compact-real.out 2>&1

echo "exit=$?  bytes=$(wc -c < /tmp/compact-real.out)"
echo
echo "=== 折叠事件 ==="
"$BIN" replay --workspace demo --json \
  | grep '"kind":"compact"' \
  | sed -e 's/.*"beforeTokens":\([0-9]*\).*"afterTokens":\([0-9]*\).*/  折叠: \1 → \2 tok/' || echo "（没有触发折叠）"
echo
echo "=== 每轮对话 ==="
"$BIN" replay --workspace demo | grep -E 'turn/start'
