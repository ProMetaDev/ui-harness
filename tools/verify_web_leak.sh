#!/usr/bin/env bash
# 对抗检查：日志已经脱敏了，但网页端的 /snapshot（宣传为"可分享"）与 SSE 还带原文吗？
#
# 现状（2026-11-20，commit 5043cbc）：**带**。journal 0 命中，SSE 与 snapshot 各 1 命中。
# 所以这个脚本现在**故意以非零退出**，作为修复前的回归基线。
# 修好之后在 tools/debug.sh 里加：
#   run "网页端也不带明文（SSE + /snapshot）" bash tools/verify_web_leak.sh
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

WS=/tmp/ui-web-leak
PORT=$(ui_free_port)
rm -rf "$WS"; mkdir -p "$WS"
CANARY="AKIAWEBLEAKCANARY123456"  # allowlist secret（故意写成密钥形状的测试夹具）

cat > "$WS/sc.json" <<JSON
{"model":"scripted","steps":[
  {"content":"读一下","toolCalls":[{"name":"shell","args":{"command":"echo $CANARY"}}]},
  {"content":"done"}]}
JSON

"$UI_BIN" serve --workspace "$WS" --port "$PORT" --mock "$WS/sc.json" --yes --token off \
  >/tmp/ui-webleak.log 2>&1 &
PID=$!
trap 'kill $PID 2>/dev/null; wait $PID 2>/dev/null' EXIT

if ! ui_wait_port "$PORT" 15; then
  echo "FAIL 服务起不来"
  exit 1
fi
BASE="http://127.0.0.1:$PORT"

( curl -sN --max-time 4 "$BASE/api/events" > /tmp/ui-sse.txt ) 2>/dev/null &
curl -s -X POST "$BASE/api/say" -H 'content-type: application/json' -d '{"text":"跑一下"}' >/dev/null
sleep 3
curl -s "$BASE/snapshot" > /tmp/ui-snapshot.html 2>/dev/null

journal_hits=$(grep -c "$CANARY" "$WS/.ui/journal.jsonl" 2>/dev/null || true)
sse_hits=$(grep -c "$CANARY" /tmp/ui-sse.txt 2>/dev/null || true)
snap_hits=$(grep -c "$CANARY" /tmp/ui-snapshot.html 2>/dev/null || true)
journal_hits=${journal_hits:-0}; sse_hits=${sse_hits:-0}; snap_hits=${snap_hits:-0}

fail=0
echo "journal 命中: $journal_hits（期望 0）"
echo "SSE     命中: $sse_hits（期望 0）"
echo "snapshot 命中: $snap_hits（期望 0）"
[ "$journal_hits" -eq 0 ] || { echo "FAIL 日志里有明文"; fail=1; }
[ "$sse_hits" -eq 0 ]     || { echo "FAIL SSE 里有明文（浏览器 DevTools 就能看到）"; fail=1; }
[ "$snap_hits" -eq 0 ]    || { echo "FAIL /snapshot 的 HTML 里有明文（这个文件是给人转发的）"; fail=1; }

if [ "$fail" -eq 0 ]; then
  echo "PASS 三条路径都不带明文"
else
  echo "FAIL 网页端仍未脱敏——日志干净不代表导出的快照干净"
fi
exit "$fail"
