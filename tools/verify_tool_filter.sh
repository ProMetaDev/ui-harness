#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# 工具白名单（A4）：--no-tools 时模型看不到也不该用工具；--tools 只留指定的。
set -uo pipefail
cd "$UI_ROOT"
BIN="$UI_BIN"
WS=/tmp/ui-tools
rm -rf "$WS"; mkdir -p "$WS"

cat > "$WS/sc.json" <<'EOF'
{"model":"scripted","steps":[
  {"content":"想写文件","toolCalls":[{"name":"fs_write","args":{"path":"a.txt","content":"x"}}]},
  {"content":"结束"}]}
EOF

echo "=== 默认：工具可用 ==="
"$BIN" run --task t --mock "$WS/sc.json" --workspace "$WS" --yes 2>&1 | grep -E '→|结束原因'
[ -f "$WS/a.txt" ] && echo "PASS 默认写了文件" || echo "FAIL 默认没写"

echo
echo "=== --no-tools：工具应被拒 ==="
rm -rf "$WS/.ui" "$WS/a.txt"
"$BIN" run --task t --mock "$WS/sc.json" --workspace "$WS" --yes --no-tools 2>&1 | grep -E '→|结束原因' | head -3
[ -f "$WS/a.txt" ] && echo "FAIL 关掉工具仍然写了文件" || echo "PASS 关掉工具后没写文件"

echo
echo "=== --tools fs_read：fs_write 应被拒 ==="
rm -rf "$WS/.ui" "$WS/a.txt"
"$BIN" run --task t --mock "$WS/sc.json" --workspace "$WS" --yes --tools fs_read 2>&1 | grep -E '→|结束原因' | head -3
[ -f "$WS/a.txt" ] && echo "FAIL 白名单外仍写了文件" || echo "PASS 白名单外被拒"

