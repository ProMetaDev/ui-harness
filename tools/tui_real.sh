#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# 真模型 + 全屏 TUI：模型会请求跑命令，审批浮层里按 y 放行，最后 q 退出。
set -uo pipefail
cd "$UI_ROOT"
export CARGO_TARGET_DIR="$UI_TARGET_DIR"
export UI_MODEL_CMD="bash tools/_node.sh tools/deepseek.mjs"
BIN="$CARGO_TARGET_DIR/release/ui"

cargo build --release --offline 2>&1 | tail -1

cat > demo/greet.js <<'EOF'
export function greet(name) {
  return `hi ${name}`;
}

console.log(greet("loom"));
console.log(greet(""));
EOF
rm -rf demo/.ui demo/.loom

echo "=== TUI 一轮（模型会请求执行命令）==="
# a = 本会话总是允许，避免第二次审批时无人按键；q = 退出
printf 'aq' | timeout 300 script -qec \
  "$BIN tui --task-file demo/task2.txt --workspace demo --max-calls 8" /dev/null > /tmp/tui-real.out 2>&1
echo "exit=$?  raw bytes=$(wc -c < /tmp/tui-real.out)"
echo "approval drawn: $(grep -c '需要批准' /tmp/tui-real.out || true)"
echo "draws:          $(grep -o $'\x1b\[[0-9]*;1H' /tmp/tui-real.out | wc -l)"

echo
echo "=== 最终文件 ==="
cat demo/greet.js

echo
echo "=== 导出轨迹 ==="
"$BIN" export --workspace demo --html out/trace.html
