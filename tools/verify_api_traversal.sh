#!/usr/bin/env bash
# 独立验证：/api/tree 是否能读工作区外的目录（路径穿越）。
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

WS=/tmp/ui-api-traversal
PORT=$(ui_free_port)
rm -rf "$WS"; mkdir -p "$WS/secrets"
echo '{"api_key":"sk-LEAK-API"}' > "$WS/secrets/astra.json"
echo hi > "$WS/readme.txt"

cat > "$WS/scenario.json" <<'JSON'
{"model":"scripted","steps":[{"content":"ok"}]}
JSON
"$UI_BIN" serve --workspace "$WS" --port "$PORT" --mock "$WS/scenario.json" --yes --token off >/tmp/ui-serve.log 2>&1 &
PID=$!
ui_wait_port "$PORT" 10 || { echo "服务起不来"; kill $PID 2>/dev/null; exit 1; }

TOKEN=$(grep -oE 'token=[A-Za-z0-9]+' /tmp/ui-serve.log | head -1 | cut -d= -f2)
AUTH=""
[ -n "$TOKEN" ] && AUTH="?token=$TOKEN"

probe() { # label, path, needle
  local label="$1" p="$2" needle="$3"
  local body
  body=$(curl -s --max-time 5 "http://127.0.0.1:$PORT/api/tree?path=$p" 2>/dev/null)
  if printf '%s' "$body" | grep -q -- "$needle"; then
    echo "LEAKED   $label（body 里出现 $needle）: $(printf '%s' "$body" | head -c 120)"
    return 1
  fi
  echo "BLOCKED  $label"
  return 0
}

fail=0
probe "/api/tree?path=/etc"       "/etc"      "passwd"   || fail=1
probe "/api/tree?path=secrets"    "secrets"   "astra"    || fail=1
probe "/api/tree?path=../"        "../"       "etc"      || fail=1

kill $PID 2>/dev/null
wait $PID 2>/dev/null
echo
[ "$fail" -eq 0 ] && echo "PASS /api/tree 没有穿越" || echo "FAIL /api/tree 可穿越（见上）"
exit "$fail"
