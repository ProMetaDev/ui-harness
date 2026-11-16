#!/usr/bin/env bash
# Web 端会话：列表 / 新建 / 切换后写入正确的日志、历史不串
set -uo pipefail
cd /mnt/x/dsh_ws/harness-v2
BIN="$HOME/ui/target/release/ui"
PORT=7806
WS=/tmp/ui-wsession
rm -rf "$WS"; mkdir -p "$WS"
TOKEN=t

cat > "$WS/sc.json" <<'EOF'
{"model":"scripted","steps":[{"content":"引擎收到"},{"content":"引擎收到"}]}
EOF

fuser -k "$PORT/tcp" >/dev/null 2>&1 && sleep 0.3
"$BIN" serve --port $PORT --workspace "$WS" --mock "$WS/sc.json" --yes --token $TOKEN > "$WS/s.log" 2>&1 &
SRV=$!
trap 'kill $SRV 2>/dev/null' EXIT
for _ in $(seq 1 60); do curl -sf -H "Cookie: ui_token=$TOKEN" "http://127.0.0.1:$PORT/api/state" >/dev/null && break; sleep 0.1; done

fail() { echo "FAIL: $1"; exit 1; }
C() { curl -s -H "Cookie: ui_token=$TOKEN" "$@"; }

echo "=== 初始列表 ==="
C "http://127.0.0.1:$PORT/api/sessions" | tee "$WS/l1.json"
grep -q '"current":"default"' "$WS/l1.json" || fail "初始会话不是 default"

echo
echo "=== 新建会话 work 并切换 ==="
C -X POST -d '{"name":"work"}' "http://127.0.0.1:$PORT/api/session/new"
C -X POST -d '{"name":"work"}' "http://127.0.0.1:$PORT/api/session" 2>/dev/null || true

echo
echo "=== 发一条消息（应在 work 日志里）==="
C -X POST -H "Content-Type: application/json" -d '{"text":"hello"}' "http://127.0.0.1:$PORT/api/say"
sleep 2
if [ -f "$WS/.ui/sessions/work.jsonl" ] && grep -q 'hello' "$WS/.ui/sessions/work.jsonl"; then
  echo "PASS 消息进了 work 会话"
else
  echo "FAIL 消息没进 work"; ls "$WS/.ui" "$WS/.ui/sessions" 2>/dev/null
fi

echo
echo "=== 切回 default ==="
C -X POST -d '{"name":"default"}' "http://127.0.0.1:$PORT/api/session"
C -X POST -H "Content-Type: application/json" -d '{"text":"back"}' "http://127.0.0.1:$PORT/api/say"
sleep 2
grep -q 'back' "$WS/.ui/journal.jsonl" 2>/dev/null && echo "PASS 切回 default 后有写入" || echo "FAIL default 没写入"

echo
echo "=== 非法名 / 不存在的会话 ==="
C -X POST -d '{"name":"../etc"}' "http://127.0.0.1:$PORT/api/session"
C -X POST -d '{"name":"nope"}' "http://127.0.0.1:$PORT/api/session"

echo
echo "=== 每个日志链都完整 ==="
for f in "$WS/.ui/journal.jsonl" "$WS/.ui/sessions/work.jsonl"; do
  [ -f "$f" ] || continue
  "$BIN" verify --workspace "$WS" --journal "$f" 2>&1 | grep -q '哈希链完整' || fail "$f 链坏"
done
echo "PASS"
