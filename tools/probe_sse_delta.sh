#!/usr/bin/env bash
# 手工验收：起 --mock 的 ui serve，确认 /api/events 真的推送 {"type":"delta"}，
# 并且最终 assistant 事件仍在。只用于本地验证，不接入 debug.sh。
set -uo pipefail
BIN="${UI_BIN:-$HOME/ui/target/release/ui}"
PORT="${1:-7810}"
BASE="http://127.0.0.1:$PORT"
WS=/tmp/ui-delta
rm -rf "$WS"
mkdir -p "$WS"

cat > "$WS/s.json" <<'EOF'
{"model":"scripted","steps":[{"content":"hello delta"},{"content":"done"}]}
EOF

"$BIN" serve --port "$PORT" --workspace "$WS" --mock "$WS/s.json" --yes >/tmp/delta-serve.log 2>&1 &
SRV=$!
trap 'kill $SRV 2>/dev/null' EXIT

for _ in $(seq 1 60); do
  curl -sf "$BASE/api/state" >/dev/null && break
  sleep 0.1
done

(curl -sN --max-time 8 "$BASE/api/events" > "$WS/events.txt") &
sleep 0.3
curl -s -X POST "$BASE/api/say" -H 'content-type: application/json' -d '{"text":"say hi"}'
echo
sleep 3
kill "$SRV" 2>/dev/null
wait "$SRV" 2>/dev/null

echo "--- SSE events ---"
cat "$WS/events.txt"

fail=0
grep -q '"type":"delta"' "$WS/events.txt" || { echo "FAIL 没有 delta 事件"; fail=1; }
grep -q '"type":"assistant"' "$WS/events.txt" || { echo "FAIL 没有最终 assistant 事件"; fail=1; }
grep -q '"type":"turn"' "$WS/events.txt" || { echo "FAIL 没有 turn 事件"; fail=1; }
[ "$fail" -eq 0 ] && echo "PASS delta + assistant 都在 SSE 里"
exit "$fail"
