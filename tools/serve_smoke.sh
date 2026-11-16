#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# Web 用户端冒烟：起服务 → 取页面 → 发消息 → 收事件 → 收工。
set -uo pipefail
cd "$UI_ROOT"
BIN="$UI_BIN"
PORT=${1:-$(ui_free_port 17800)}
BASE="http://127.0.0.1:$PORT"
WS=/tmp/ui-web
rm -rf "$WS"; mkdir -p "$WS"
cp demo/greet.js "$WS/greet.js"

cat > "$WS/scenario.json" <<'EOF'
{"model":"scripted","steps":[
  {"content":"先读一下 greet.js。","toolCalls":[{"name":"fs_read","args":{"path":"greet.js"}}]},
  {"content":"空名字会拼出 \"hi \"，应该用兜底。"}
]}
EOF

# ?????????????????(?????)
fuser -k "$PORT/tcp" >/dev/null 2>&1 && sleep 0.3
"$BIN" serve --port "$PORT" --workspace "$WS" --mock "$WS/scenario.json" --yes --token off >/tmp/serve.log 2>&1 &
SRV=$!
trap 'kill $SRV 2>/dev/null' EXIT

for _ in $(seq 1 60); do
  curl -sf "$BASE/api/state" >/dev/null && break
  sleep 0.1
done

fail=0
check() {
  if [ "$1" = "1" ]; then echo "PASS  $2"; else echo "FAIL  $2"; fail=1; fi
}

PAGE=$(curl -s "$BASE/")
case "$PAGE" in *U:I*) check 1 'GET / 返回页面' ;; *) check 0 'GET / 返回页面' ;; esac

STATE=$(curl -s "$BASE/api/state")
case "$STATE" in *busy*) check 1 'GET /api/state' ;; *) check 0 'GET /api/state' ;; esac

# 订阅事件流（后台），再发一条消息
( curl -sN --max-time 6 "$BASE/api/events" > "$WS/events.txt" ) &
sleep 0.3
SAY=$(curl -s -X POST "$BASE/api/say" -H 'content-type: application/json' -d '{"text":"greet 的空名字怎么处理？"}')
case "$SAY" in *'"ok":true'*) check 1 'POST /api/say' ;; *) check 0 'POST /api/say' ;; esac

sleep 3
for want in '"type":"user"' '"type":"delta"' '"type":"assistant"' '"type":"tool"' '"type":"turn"'; do
  grep -q "$want" "$WS/events.txt" && check 1 "SSE 收到 $want" || check 0 "SSE 收到 $want"
done
# 工具行里应该有文件内容，而不是空
grep -q 'greet' "$WS/events.txt" && check 1 '工具结果有内容' || check 0 '工具结果有内容'

kill $SRV 2>/dev/null
wait $SRV 2>/dev/null
exit $fail
