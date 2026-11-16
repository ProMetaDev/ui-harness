#!/usr/bin/env bash
# 前端新能力自测：停止按钮、工具输出展开、断线重连按 since 去重。
set -uo pipefail
cd /mnt/x/dsh_ws/harness-v2
BIN="$HOME/ui/target/release/ui"
PORT=${1:-7811}
BASE="http://127.0.0.1:$PORT"
WS=/tmp/ui-web-features
rm -rf "$WS"; mkdir -p "$WS"
printf 'export function greet(name) {\n  return name ? `hi ${name}` : "hi ";\n}\n' > "$WS/greet.js"

cat > "$WS/slow.json" <<'EOF'
{"model":"scripted","steps":[
  {"content":"跑个慢命令","toolCalls":[{"name":"shell","args":{"command":"sleep 5","timeoutMs":20000}}]},
  {"content":"不该到这里"}]}
EOF

"$BIN" serve --port "$PORT" --workspace "$WS" --mock "$WS/slow.json" --yes --token off >/tmp/web-features.log 2>&1 &
SRV=$!
trap 'kill $SRV 2>/dev/null' EXIT
for _ in $(seq 1 80); do curl -sf "$BASE/api/state" >/dev/null && break; sleep 0.1; done

fail=0
ok() { echo "PASS  $1"; }
no() { echo "FAIL  $1"; fail=1; }

# 1) 停止：发一条，等它进 shell，然后按停止
( curl -sN --max-time 12 "$BASE/api/events" > "$WS/events.txt" ) &
sleep 0.3
curl -s -X POST "$BASE/api/say" -H 'content-type: application/json' -d '{"text":"慢任务"}' >/dev/null
sleep 0.6
BUSY=$(curl -s "$BASE/api/state" | grep -o '"busy":[a-z]*')
[ "$BUSY" = '"busy":true' ] && ok "运行中状态可见" || no "运行中状态可见（实际 $BUSY）"

START=$(date +%s%3N)
curl -s -X POST "$BASE/api/cancel" >/dev/null
for _ in $(seq 1 60); do
  grep -q '"busy":false' <<<"$(curl -s "$BASE/api/state")" && break
  sleep 0.1
done
END=$(date +%s%3N)
ELAPSED=$((END-START))
grep -q '"busy":false' <<<"$(curl -s "$BASE/api/state")" && ok "停止后回到空闲" || no "停止后回到空闲"
[ "$ELAPSED" -lt 3000 ] && ok "停止生效耗时 ${ELAPSED}ms" || no "停止生效太慢：${ELAPSED}ms"

sleep 0.5
grep -q '"reason":"cancelled"' "$WS/events.txt" && ok "SSE 收到 cancelled" || no "SSE 收到 cancelled"
grep -q '"type":"cancel-requested"' "$WS/events.txt" && ok "SSE 收到 cancel-requested" || no "SSE 收到 cancel-requested"

# 2) 工具展开：换成读文件的剧本，拿到 tool id 再取全文
kill $SRV 2>/dev/null; wait $SRV 2>/dev/null
cat > "$WS/read.json" <<'EOF'
{"model":"scripted","steps":[
  {"content":"读一下","toolCalls":[{"name":"fs_read","args":{"path":"greet.js"}}]},
  {"content":"读完了"}]}
EOF
"$BIN" serve --port "$PORT" --workspace "$WS" --mock "$WS/read.json" --yes --token off >/tmp/web-features2.log 2>&1 &
SRV=$!
for _ in $(seq 1 80); do curl -sf "$BASE/api/state" >/dev/null && break; sleep 0.1; done
( curl -sN --max-time 8 "$BASE/api/events" > "$WS/events2.txt" ) &
sleep 0.3
curl -s -X POST "$BASE/api/say" -H 'content-type: application/json' -d '{"text":"读文件"}' >/dev/null
sleep 2
TOOL_ID=$(grep -o '"type":"tool"[^}]*"id":[0-9]*' "$WS/events2.txt" | grep -o '[0-9]*$' | head -1)
[ -n "$TOOL_ID" ] && ok "tool 事件带 id=$TOOL_ID" || no "tool 事件带 id"
FULL=$(curl -s "$BASE/api/tool/$TOOL_ID")
case "$FULL" in *greet*) ok "GET /api/tool/<id> 能取全文" ;; *) no "GET /api/tool/<id> 能取全文" ;; esac
case "$FULL" in *'"ok":true'*) ok "工具结果 is ok" ;; *) no "工具结果 is ok" ;; esac

# 3) 断线重连按 since 去重
LAST=$(grep -o '"seq":[0-9]*' "$WS/events2.txt" | tail -1 | grep -o '[0-9]*')
REPLAY=$(curl -s --max-time 2 "$BASE/api/events?since=$LAST" | grep -c '"seq"' || true)
[ "$REPLAY" -eq 0 ] && ok "since=末尾序号时不重放历史" || no "since 去重（重放了 $REPLAY 条）"

kill $SRV 2>/dev/null; wait $SRV 2>/dev/null
[ "$fail" -eq 0 ] && echo "== 全部通过" || echo "== 有失败项"
exit "$fail"
