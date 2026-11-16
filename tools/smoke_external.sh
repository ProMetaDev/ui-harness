#!/usr/bin/env bash
# 验证「内核 ↔ 子进程模型」这条缝：不碰网络，不需要 key。
set -uo pipefail
cd /mnt/x/dsh_ws/harness-v2
export CARGO_TARGET_DIR="$HOME/ui-target"
BIN="$CARGO_TARGET_DIR/release/ui"

cat > demo/greet.js <<'EOF'
export function greet(name) {
  return `hi ${name}`;
}

console.log(greet("loom"));
console.log(greet(""));
EOF
rm -rf demo/.ui /tmp/ui-fake-count /tmp/ui-fake-prompt.log

echo "=== 1. 适配器干跑（不联网、不打 key）==="
echo '{"prompt":"<core>hi</core>","repair":null}' | node tools/deepseek.mjs --dry | head -12

echo
echo "=== 2. 用子进程模型跑一轮真实工具调用 ==="
UI_MODEL_CMD="node tools/fake-chat.mjs" "$BIN" run \
  --task-file demo/task.txt --workspace demo --yes

echo
echo "=== 3. 结果 ==="
grep -n 'return `hi' demo/greet.js
echo
echo "=== 4. 适配器收到的快照（前 12 行）==="
head -12 /tmp/ui-fake-prompt.log
