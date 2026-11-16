#!/usr/bin/env bash
# 端到端：离线假 DeepSeek → 适配器（流式）→ 内核 → 网页端 SSE → 静态快照。
# 全程不联网、不花钱；产物落到 out/stream/。
set -uo pipefail
cd /mnt/x/dsh_ws/harness-v2

BIN="$HOME/ui/target/release/ui"
OUT="$PWD/out/stream"
PORT=${1:-18460}
KPORT=${2:-7861}
rm -rf "$OUT"; mkdir -p "$OUT/work"

openssl req -x509 -newkey rsa:2048 -nodes \
  -keyout "$OUT/key.pem" -out "$OUT/cert.pem" -days 1 \
  -subj '/CN=127.0.0.1' >/dev/null 2>&1

cat > "$OUT/providers.toml" <<EOF
[chat]
api_key = "test-key"
base_url = "https://127.0.0.1:$PORT"
model = "fake-model"
EOF

cat > "$OUT/work/greet.js" <<'EOF'
export function greet(name) {
  return name ? `hi ${name}` : "hi ";
}
EOF

# 1) 假模型服务
FAKE_PORT="$PORT" FAKE_KEY="$OUT/key.pem" FAKE_CERT="$OUT/cert.pem" \
  bash tools/_node.sh tools/fake_deepseek.mjs > "$OUT/fake.log" 2>&1 &
FAKE=$!
trap 'kill $FAKE $SRV 2>/dev/null' EXIT

for _ in $(seq 1 80); do grep -q listening "$OUT/fake.log" && break; sleep 0.1; done

# 2) 内核 + 网页端，模型进程指向这个假服务
UI_MODEL_CMD="UI_PROVIDERS_FILE=$OUT/providers.toml NODE_TLS_REJECT_UNAUTHORIZED=0 bash $PWD/tools/_node.sh $PWD/tools/deepseek.mjs" \
  "$BIN" serve --port "$KPORT" --workspace "$OUT/work" --yes > "$OUT/serve.log" 2>&1 &
SRV=$!
for _ in $(seq 1 80); do curl -sf "http://127.0.0.1:$KPORT/api/state" >/dev/null && break; sleep 0.1; done

# 3) 发一条消息，同时订阅 SSE
( curl -sN --max-time 8 "http://127.0.0.1:$KPORT/api/events" > "$OUT/events.txt" ) &
sleep 0.3
curl -s -X POST "http://127.0.0.1:$KPORT/api/say" \
  -H 'content-type: application/json' -d '{"text":"打个招呼"}' >/dev/null
sleep 4

echo "=== SSE 事件 ==="
cat "$OUT/events.txt"
echo
echo "delta 条数: $(grep -c '"type":"delta"' "$OUT/events.txt" || true)"

# 4) 静态快照（给浏览器截图用，不依赖 SSE）
curl -s "http://127.0.0.1:$KPORT/snapshot" > "$OUT/snapshot.html"
echo "快照字节数: $(wc -c < "$OUT/snapshot.html")"

kill $SRV 2>/dev/null; wait $SRV 2>/dev/null
kill $FAKE 2>/dev/null; wait $FAKE 2>/dev/null
