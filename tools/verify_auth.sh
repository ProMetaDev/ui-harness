#!/usr/bin/env bash
# 鉴权：没有 token 什么都拿不到；带 ?t= 种 Cookie 后正常；API 返回 401 JSON。
set -uo pipefail
cd /mnt/x/dsh_ws/harness-v2
BIN="$HOME/ui/target/release/ui"
PORT=7807
WS=/tmp/ui-auth
rm -rf "$WS"; mkdir -p "$WS"
TOKEN=s3cret

fuser -k "$PORT/tcp" >/dev/null 2>&1 && sleep 0.3
"$BIN" serve --port $PORT --workspace "$WS" --mock fixtures/mock.json --yes --token $TOKEN \
  --bundles "$WS/b" > "$WS/s.log" 2>&1 &
SRV=$!
trap 'kill $SRV 2>/dev/null' EXIT
for _ in $(seq 1 60); do curl -s -o /dev/null "http://127.0.0.1:$PORT/?t=$TOKEN" && break; sleep 0.1; done

fail() { echo "FAIL: $1"; exit 1; }

code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/")
[ "$code" = "401" ] || fail "不带 token 应 401，实际 $code"

code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/api/state")
[ "$code" = "401" ] || fail "API 不带 token 应 401，实际 $code"

body=$(curl -s "http://127.0.0.1:$PORT/api/state")
echo "$body" | grep -q unauthorized || fail "401 应返回 JSON：$body"

# 带 token：303 + Set-Cookie
head=$(curl -s -i "http://127.0.0.1:$PORT/?t=$TOKEN")
echo "$head" | head -1 | grep -q 303 || fail "带 token 应 303 跳转"
echo "$head" | grep -qi 'set-cookie: ui_token=' || fail "应种 Cookie"

# 用 Cookie 访问页面
code=$(curl -s -o /dev/null -w '%{http_code}' -H "Cookie: ui_token=$TOKEN" "http://127.0.0.1:$PORT/")
[ "$code" = "200" ] || fail "Cookie 访问应 200，实际 $code"

# 错误 token
code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/?t=wrong")
[ "$code" = "401" ] || fail "错误 token 应 401，实际 $code"

# --token off 时不校验
fuser -k "$PORT/tcp" >/dev/null 2>&1; sleep 0.4
"$BIN" serve --port $PORT --workspace "$WS" --mock fixtures/mock.json --yes --token off > "$WS/s2.log" 2>&1 &
SRV2=$!
for _ in $(seq 1 60); do curl -s -o /dev/null "http://127.0.0.1:$PORT/api/state" && break; sleep 0.1; done
code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/")
[ "$code" = "200" ] || fail "--token off 应放行，实际 $code"
kill $SRV2 2>/dev/null

echo "PASS 鉴权：401 / Cookie / 错误 token / --token off"
