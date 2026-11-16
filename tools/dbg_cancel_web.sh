#!/usr/bin/env bash
set -uo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"
BIN="$UI_BIN"
PORT=7822
BASE="http://127.0.0.1:$PORT"
WS=/tmp/ui-cancel-debug
rm -rf "$WS"; mkdir -p "$WS"
cat > "$WS/slow.json" <<'EOF'
{"model":"scripted","steps":[
  {"content":"跑个慢命令","toolCalls":[{"name":"shell","args":{"command":"sleep 5","timeoutMs":20000}}]},
  {"content":"不该到这里"}]}
EOF

"$BIN" serve --port "$PORT" --workspace "$WS" --mock "$WS/slow.json" --yes >/tmp/cancel-debug.log 2>&1 &
SRV=$!
trap 'kill $SRV 2>/dev/null' EXIT
for _ in $(seq 1 80); do curl -sf "$BASE/api/state" >/dev/null && break; sleep 0.1; done

curl -s -X POST "$BASE/api/say" -H 'content-type: application/json' -d '{"text":"慢任务"}' >/dev/null
sleep 0.8
echo "cancel 响应: $(curl -s -X POST "$BASE/api/cancel")"
sleep 1.5
echo "state: $(curl -s "$BASE/api/state")"
echo "--- 日志事件 ---"
grep -oE '"kind":"[a-z/]*"|"error":"[^"]*"|"durationMs":[0-9]*|"decision":"[a-z]*"' "$WS/.ui/journal.jsonl" | tail -12
echo "--- 进程 ---"
pgrep -af 'sleep 5' | head -3
