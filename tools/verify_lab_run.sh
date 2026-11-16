#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# 实验室「跑一次」：POST 触发 → 并发保护 → SSE 有 lab-run 事件 → 列表仍可读。
set -uo pipefail
cd "$UI_ROOT"
BIN="$UI_BIN"
PORT=$(ui_free_port 17800)
WS=/tmp/ui-labrun
rm -rf "$WS"; mkdir -p "$WS/bundles"

fuser -k "$PORT/tcp" >/dev/null 2>&1 && sleep 0.3
"$BIN" serve --port $PORT --workspace "$WS" --mock fixtures/mock.json --yes --token off \
  --bundles "$WS/bundles" --bench-cmd "sleep 0.4; echo done" > "$WS/server.log" 2>&1 &
SRV=$!
trap 'kill $SRV 2>/dev/null' EXIT
sleep 1.5

fail() { echo "FAIL: $1"; exit 1; }

# 先挂上 SSE，才能抓到事件
( curl -sN --max-time 4 "http://127.0.0.1:$PORT/api/events?since=0" > "$WS/sse.log" 2>&1 ) &
sleep 0.3

first=$(curl -s -X POST "http://127.0.0.1:$PORT/api/lab/run")
[ "$first" = '{"ok":true}' ] || fail "第一次 POST 应该成功，实际 $first"

second=$(curl -s -X POST "http://127.0.0.1:$PORT/api/lab/run")
echo "$second" | grep -q '已经在跑了' || fail "并发保护失效，实际 $second"

sleep 2
grep -q 'lab-run' "$WS/sse.log" || fail "SSE 里没有 lab-run 事件"
grep -q '"phase":"end"' "$WS/sse.log" || fail "SSE 里没有 end 事件"

list=$(curl -s "http://127.0.0.1:$PORT/api/lab/bundles")
echo "$list" | grep -q '"bundles":\[' || fail "列表接口异常：$list"

echo "PASS 跑一次 + 并发保护 + SSE 事件"
