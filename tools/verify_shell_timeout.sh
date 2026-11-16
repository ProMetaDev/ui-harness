#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# 验证：超时能连后代进程一起收掉（之前 `sleep 3 & wait` 会活到自然结束）。
set -uo pipefail
cd "$UI_ROOT"
BIN="$UI_BIN"
BASE=/tmp/ui-shelltimeout
rm -rf "$BASE"; mkdir -p "$BASE/work"

cat > "$BASE/work/sc.json" <<'EOF'
{"model":"scripted","steps":[
  {"content":"跑个带后台进程的命令","toolCalls":[
    {"name":"shell","args":{"command":"sleep 3 & wait","timeoutMs":200}}]},
  {"content":"结束"}]}
EOF

START=$(date +%s%3N)
"$BIN" run --task "试试超时" --mock "$BASE/work/sc.json" --workspace "$BASE/work" --yes 2>&1 | grep -E '→ shell|结束原因' || true
END=$(date +%s%3N)
echo "实测总耗时: $((END-START))ms  （超时设的 200ms，应该远小于 3000ms）"
echo "日志里的 timedOut: $(grep -o '"timedOut":[a-z]*' "$BASE/work/.ui/journal.jsonl" | tail -1)"
echo "还有没有 sleep 残留: $(pgrep -f 'sleep 3' | wc -l)"
