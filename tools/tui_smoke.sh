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

echo
echo "=== 3. 宽终端右栏（120x30）：当前步 / 快照 / 决策 / 账本 ==="
rm -rf /tmp/ui-tui-wide
cp -a demo /tmp/ui-tui-wide
rm -rf /tmp/ui-tui-wide/.ui /tmp/ui-tui-wide/.loom
printf 'fix the empty-name bug\r\x1b' | timeout 40 script -qec \
  "stty rows 30 cols 120; $BIN tui --mock $PWD/fixtures/scenarios/fix-bug.json --workspace /tmp/ui-tui-wide --yes" \
  /dev/null > /tmp/tui3.out 2>&1
echo "exit=$?"
RAIL=$(grep -c '当前步' /tmp/tui3.out || true)
LEDGER=$(grep -c '账本' /tmp/tui3.out || true)
# `stty size` 读不到 tty 时 TUI 会退化 80×24，右栏（≥100 列）就不会出现——这条能抓住。
echo "rail frames: $RAIL · ledger: $LEDGER"
if [ "$RAIL" -lt 1 ] || [ "$LEDGER" -lt 1 ]; then
  echo "FAIL: 120 列下没有出现右栏（检查 terminal_size 是否真的读到了 tty 尺寸）"
  exit 1
fi
