#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# CRLF 文件上的 patch：这是 Windows 实机发现的问题，先在 Linux 上复现
set -uo pipefail
cd "$UI_ROOT"
BIN="$UI_BIN"
WS=/tmp/ui-crlf
rm -rf "$WS"; mkdir -p "$WS"

printf 'export function add(a,b){ return a+b; }\r\nexport function mul(a,b){ return a*b; }\r\n' > "$WS/a.js"
echo "原文（od 看换行）:"
od -c "$WS/a.js" | head -3

cat > "$WS/sc.json" <<'EOF'
{"model":"scripted","steps":[
  {"content":"打补丁","toolCalls":[{"name":"patch","args":{"path":"a.js","diff":"@@ -1,1 +1,2 @@\n export function add(a,b){ return a+b; }\n+// patched\n"}}]},
  {"content":"完"}]}
EOF
"$BIN" run --task t --mock "$WS/sc.json" --workspace "$WS" --yes 2>&1 | grep -E '→'

echo
echo "结果:"
od -c "$WS/a.js" | head -4
grep -q 'patched' "$WS/a.js" && echo "PASS CRLF 文件也能打补丁" || echo "FAIL 补丁没生效"
