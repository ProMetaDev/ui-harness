#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# MCP 端到端：拉起 python mock server → 模型调 mcp_call → 工具结果回来 → 决策留痕。
set -uo pipefail
cd "$UI_ROOT"
BIN="$UI_BIN"
WS=/tmp/ui-mcp
rm -rf "$WS"; mkdir -p "$WS"

cat > "$WS/sc.json" <<'EOF'
{"model":"scripted","steps":[
  {"content":"调外部工具","toolCalls":[{"name":"mcp_call","args":{"server":"demo","tool":"echo","args":{"text":"你好 MCP"}}}]},
  {"content":"调加法","toolCalls":[{"name":"mcp_call","args":{"server":"demo","tool":"add","args":{"a":2,"b":40}}}]},
  {"content":"完成"}]}
EOF

echo "=== 带 MCP 跑一轮 ==="
"$BIN" run --task "用外部工具算个和" --mock "$WS/sc.json" --workspace "$WS" --yes \
  --mcp fixtures/mcp/demo.json 2>&1 | grep -E '→|结束原因'

echo
echo "=== 日志里的决策（效果集 + 审批）==="
grep -o '"kind":"tool/request"[^}]*' "$WS/.ui/journal.jsonl" | head -2

echo
echo "=== 不配 MCP 时（同一个剧本，应明确报错）==="
rm -rf "$WS/.ui"
"$BIN" run --task "再试一次" --mock "$WS/sc.json" --workspace "$WS" --yes 2>&1 | grep -E '→|结束原因' | head -3
