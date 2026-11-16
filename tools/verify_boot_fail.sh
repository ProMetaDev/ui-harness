#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# 验证开机检查的失败路径：损坏的 journal 应让 /api/ready 报 ok:false 并降级到临时日志。
set -uo pipefail
cd "$UI_ROOT"
BIN="$UI_BIN"
WS=/tmp/ui-boot-fail
PORT=7840
rm -rf "$WS"; mkdir -p "$WS/.ui"

# 造一个中间损坏的日志：两条记录，改掉第一条
printf '%s\n' '{"seq":1,"ts":1,"kind":"note","text":"a","prev":"0000000000000000000000000000000000000000000000000000000000000000","hash":"deadbeef"}' \
             '{"seq":2,"ts":2,"kind":"note","text":"b","prev":"deadbeef","hash":"cafebabe"}' > "$WS/.ui/journal.jsonl"

"$BIN" serve --port "$PORT" --workspace "$WS" --mock fixtures/mock.json --yes >/tmp/boot-fail.log 2>&1 &
SRV=$!
trap 'kill $SRV 2>/dev/null' EXIT
for _ in $(seq 1 60); do curl -sf "http://127.0.0.1:$PORT/api/ready" >/dev/null && break; sleep 0.1; done

READY=$(curl -s "http://127.0.0.1:$PORT/api/ready")
echo "$READY"
echo
echo "$READY" | grep -q '"ok":false' && echo "PASS 失败被如实报告（ok:false）" || echo "FAIL 没有报告失败"
echo "$READY" | grep -q '"degraded":true' && echo "PASS 降级标记存在" || echo "FAIL 缺降级标记"
echo "$READY" | grep -q '"id":"journal","ok":false' && echo "PASS journal 那一步标红" || echo "FAIL journal 步骤没标失败"

kill $SRV 2>/dev/null; wait $SRV 2>/dev/null
