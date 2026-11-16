#!/usr/bin/env bash
# 用 script(1) 造 pty，端到端验证全屏 TUI：
#   1) 进/出备用屏；2) 画帧；3) 输入一行并发送；4) 审批键 a；5) Esc 退出。
set -uo pipefail
cd /mnt/x/dsh_ws/harness-v2
export CARGO_TARGET_DIR="$HOME/ui-target"
BIN="$CARGO_TARGET_DIR/release/ui"

cargo build --release --offline 2>&1 | tail -1

cat > demo/greet.js <<'EOF'
export function greet(name) {
  return `hi ${name}`;
}

console.log(greet("loom"));
console.log(greet(""));
EOF

echo "=== 1. 启动任务 + Esc 退出 ==="
rm -rf demo/.ui demo/.loom
printf '\x1b' | timeout 25 script -qec \
  "$BIN tui --mock fixtures/scenarios/fix-bug.json --task-file demo/task.txt --workspace demo --yes" \
  /dev/null > /tmp/tui1.out 2>&1
echo "exit=$?  bytes=$(wc -c < /tmp/tui1.out)"
echo "alt screen: $(grep -c $'\x1b\[?1049h' /tmp/tui1.out) in / $(grep -c $'\x1b\[?1049l' /tmp/tui1.out) out"

echo
echo "=== 2. 键盘输入一行 + 审批 a + Esc ==="
rm -rf demo/.ui
printf '跑一下 greet.js\ra\x1b' | timeout 40 script -qec \
  "$BIN tui --mock fixtures/scenarios/tui-shell.json --workspace demo" /dev/null > /tmp/tui2.out 2>&1
echo "exit=$?"
echo "approval drawn: $(grep -c '需要批准' /tmp/tui2.out || true)"
echo "typed line echoed: $(grep -c '跑一下' /tmp/tui2.out || true)"
sed -e 's/\x1b\[[0-9;?]*[a-zA-Z]//g' /tmp/tui2.out | tr -d '\r' | tail -6
