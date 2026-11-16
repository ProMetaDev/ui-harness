#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# 验证「模式只改严格度」这件事在真实运行里成立：
#   1) quiet 模式跑一轮（命令自动放行）
#   2) 用 strict 重判同一份日志 → 应该指出那些执行动作会被收紧
#   3) 日志本身完整（哈希链可验）
set -uo pipefail
cd "$UI_ROOT"
BIN="$UI_BIN"
WS=/tmp/ui-mode
rm -rf "$WS"; mkdir -p "$WS"
printf 'export function greet(n){ return n ? `hi ${n}` : "hi "; }\n' > "$WS/greet.js"

cat > "$WS/sc.json" <<'EOF'
{"model":"scripted","steps":[
  {"content":"读一下","toolCalls":[{"name":"fs_read","args":{"path":"greet.js"}}]},
  {"content":"写一下","toolCalls":[{"name":"fs_write","args":{"path":"out.txt","content":"x"}}]},
  {"content":"跑个命令","toolCalls":[{"name":"shell","args":{"command":"echo hi"}}]},
  {"content":"结束"}]}
EOF

echo "=== quiet 模式跑一轮（--yes 让它不真的等人）==="
"$BIN" run --task "试试模式" --mock "$WS/sc.json" --workspace "$WS" --mode quiet --yes 2>&1 | grep -E '→|结束原因'

echo
echo "=== 日志里记没记 mode ==="
grep -o '"kind":"turn/start"[^}]*' "$WS/.ui/journal.jsonl" | head -1

echo
echo "=== 按 strict 重判 ==="
"$BIN" audit --workspace "$WS" --as strict

echo
echo "=== JSON 形态 ==="
"$BIN" audit --workspace "$WS" --as strict --json

echo
echo "=== 日志自检 ==="
"$BIN" verify --workspace "$WS" | head -3
