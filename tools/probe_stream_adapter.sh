#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# 离线验证 tools/deepseek.mjs 的流式协议：本地 HTTPS 假服务 + 自签证书。
# 不调用真实 API、不需要真 key。UI_PROVIDERS_FILE 让适配器读测试配置。
set -uo pipefail
cd "$UI_ROOT"

PORT=${1:-18443}
TMP=/tmp/ui-adapter-stream
rm -rf "$TMP"
mkdir -p "$TMP"

openssl req -x509 -newkey rsa:2048 -nodes \
  -keyout "$TMP/key.pem" -out "$TMP/cert.pem" -days 1 \
  -subj '/CN=127.0.0.1' >/dev/null 2>&1

cat > "$TMP/providers.toml" <<EOF
[chat]
api_key = "test-key"
base_url = "https://127.0.0.1:$PORT"
model = "fake-model"
EOF

cat > "$TMP/in.json" <<'EOF'
{"prompt":"请回答","repair":null}
EOF

FAKE_PORT="$PORT" FAKE_KEY="$TMP/key.pem" FAKE_CERT="$TMP/cert.pem" \
  bash tools/_node.sh tools/fake_deepseek.mjs > "$TMP/server.log" 2>&1 &
SRV=$!
trap 'kill $SRV 2>/dev/null' EXIT

for _ in $(seq 1 80); do
  grep -q 'listening' "$TMP/server.log" && break
  sleep 0.1
done

fail=0

run_adapter() {
  local out="$1"
  shift
  env UI_PROVIDERS_FILE="$TMP/providers.toml" NODE_TLS_REJECT_UNAUTHORIZED=0 "$@" \
    bash tools/_node.sh tools/deepseek.mjs < "$TMP/in.json" > "$out" 2> "$out.err"
  echo "adapter exit=$? -> $out"
}

# 1) 默认流式：>=2 行 delta，最后一行是最终结果
run_adapter "$TMP/stream.jsonl"
LINES=$(wc -l < "$TMP/stream.jsonl")
DELTAS=$(grep -c '"delta"' "$TMP/stream.jsonl" || true)
LAST=$(tail -1 "$TMP/stream.jsonl")
echo "stream lines=$LINES deltas=$DELTAS"
if [ "$DELTAS" -lt 2 ]; then echo "FAIL 流式模式没有逐行 delta"; fail=1; fi
if [ "$(grep -c '"delta"' <<<"$LAST")" -ne 0 ]; then echo "FAIL 最后一行仍是 delta"; fail=1; fi
echo "$LAST" | grep -q '"content":"流式你好"' || { echo "FAIL 最终结果不是解析后的 content"; fail=1; }
echo "$LAST" | grep -q '"usageTokens":12' || { echo "FAIL 最终结果缺 usageTokens"; fail=1; }
# 展示用的 delta 必须是**人话**，不能是协议 JSON 的碎片
if grep -o '"delta":"[^"]*"' "$TMP/stream.jsonl" | grep -q '[{}]'; then
  echo "FAIL delta 里出现了协议 JSON 的括号（应只推 content 文本）"
  fail=1
fi
JOINED=$(grep -o '"delta":"[^"]*"' "$TMP/stream.jsonl" | sed -e 's/^"delta":"//' -e 's/"$//' | tr -d '\n')
echo "delta 拼起来 = [$JOINED]"
[ "$JOINED" = "流式你好" ] || { echo "FAIL delta 拼起来不等于完整 content"; fail=1; }

# 2) UI_STREAM=0：只有最终一行
run_adapter "$TMP/nostream.jsonl" UI_STREAM=0
LINES=$(wc -l < "$TMP/nostream.jsonl")
DELTAS=$(grep -c '"delta"' "$TMP/nostream.jsonl" || true)
echo "UI_STREAM=0 lines=$LINES deltas=$DELTAS"
if [ "$DELTAS" -ne 0 ]; then echo "FAIL UI_STREAM=0 仍输出 delta"; fail=1; fi
tail -1 "$TMP/nostream.jsonl" | grep -q '"content":"流式你好"' || { echo "FAIL UI_STREAM=0 最终结果不对"; fail=1; }

# 3) 输入显式 stream:false：同样只有最终一行
cat > "$TMP/in-nostream.json" <<'EOF'
{"prompt":"请回答","repair":null,"stream":false}
EOF
env UI_PROVIDERS_FILE="$TMP/providers.toml" NODE_TLS_REJECT_UNAUTHORIZED=0 \
  bash tools/_node.sh tools/deepseek.mjs < "$TMP/in-nostream.json" > "$TMP/in-nostream.jsonl" 2> "$TMP/in-nostream.err"
echo "input stream:false exit=$? lines=$(wc -l < "$TMP/in-nostream.jsonl")"
if [ "$(wc -l < "$TMP/in-nostream.jsonl")" -ne 1 ]; then echo "FAIL input.stream=false 应只输出一行"; fail=1; fi
if grep -q '"delta"' "$TMP/in-nostream.jsonl"; then echo "FAIL input.stream=false 仍输出 delta"; fail=1; fi

# 4) 各次输出的末行也必须是协议结果（防止把 delta 当成最终结果）
for f in "$TMP/stream.jsonl" "$TMP/nostream.jsonl" "$TMP/in-nostream.jsonl"; do
  tail -1 "$f" | grep -q '"toolCalls"' || { echo "FAIL $f 末行不是完整结果"; fail=1; }
done

# 5) 内核端到端（需要 release 二进制）：UI_MODEL_CMD 指向本适配器，
#    用 ui serve 的 SSE 看内核是否把多行 delta 转成多个 delta 事件。
BIN="${UI_BIN:-"$UI_BIN"}"
if [ -x "$BIN" ]; then
  KWS=/tmp/ui-adapter-kernel
  KPORT=$((PORT + 1))
  KBASE="http://127.0.0.1:$KPORT"
  rm -rf "$KWS"
  mkdir -p "$KWS"
  UI_MODEL_CMD="UI_PROVIDERS_FILE=$TMP/providers.toml NODE_TLS_REJECT_UNAUTHORIZED=0 bash $PWD/tools/_node.sh $PWD/tools/deepseek.mjs" \
    "$BIN" serve --port "$KPORT" --workspace "$KWS" --yes > "$TMP/kernel-serve.log" 2>&1 &
  KSRV=$!
  for _ in $(seq 1 80); do
    curl -sf "$KBASE/api/state" >/dev/null && break
    sleep 0.1
  done
  (curl -sN --max-time 10 "$KBASE/api/events" > "$TMP/kernel-events.txt") &
  sleep 0.3
  curl -s -X POST "$KBASE/api/say" -H 'content-type: application/json' -d '{"text":"say hi"}' >/dev/null
  sleep 4
  kill "$KSRV" 2>/dev/null
  wait "$KSRV" 2>/dev/null
  KD=$(grep -c '"type":"delta"' "$TMP/kernel-events.txt" || true)
  echo "kernel SSE deltas=$KD"
  if [ "$KD" -lt 2 ]; then echo "FAIL 内核没有把适配器的多行 delta 转成多个 SSE delta"; fail=1; fi
  grep -q '"type":"assistant","text":"流式你好"' "$TMP/kernel-events.txt" \
    || { echo "FAIL 内核的最终 assistant 不是解析后的 content"; fail=1; }
else
  echo "SKIP 内核端到端（找不到 $BIN）"
fi

[ "$fail" -eq 0 ] && echo "PASS 适配器流式协议（离线假服务）"
exit "$fail"
