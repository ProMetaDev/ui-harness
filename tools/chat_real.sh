#!/usr/bin/env bash
# 真模型多轮对话：第一轮看问题，第二轮让它按方案修 —— 验证跨轮历史。
set -uo pipefail
cd /mnt/x/dsh_ws/harness-v2
export CARGO_TARGET_DIR="$HOME/ui-target"
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

# --yes：自动批准，省得在两次审批上赌按键顺序
printf '跑一下 greet.js，看看空名字现在输出什么\r按 A 方案修：空名字返回 hi there；改完重跑验证\r\x1b' \
  | timeout 300 script -qec \
      "$BIN tui --workspace demo --yes --max-calls 10" /dev/null > /tmp/chat-real.out 2>&1

echo "exit=$?  bytes=$(wc -c < /tmp/chat-real.out)"
echo
echo "=== 最终文件 ==="
cat demo/greet.js
echo
echo "=== 日志 ==="
"$BIN" replay --workspace demo --json | grep -o '"kind":"[^"]*"' | sort | uniq -c
