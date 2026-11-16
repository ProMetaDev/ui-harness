#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# 输入血缘：读过的数据也留指纹。数据换版本 → 校验必须拒绝。
set -uo pipefail
cd "$UI_ROOT"
BIN="$UI_BIN"
WS=/tmp/ui-lineage
BUNDLE=/tmp/ui-lineage-bundle
rm -rf "$WS" "$BUNDLE"; mkdir -p "$WS"

printf 'x,y\n1,0\n2,1\n' > "$WS/data.csv"
cat > "$WS/sc.json" <<'EOF'
{"model":"scripted","steps":[
  {"content":"读数据","toolCalls":[{"name":"fs_read","args":{"path":"data.csv"}}]},
  {"content":"写结论","toolCalls":[{"name":"fs_write","args":{"path":"result.txt","content":"acc=0.8"}}]},
  {"content":"好了"}]}
EOF

echo "=== 跑一轮（读 data.csv，写 result.txt）==="
"$BIN" run --task "读数据算结果" --mock "$WS/sc.json" --workspace "$WS" --yes 2>&1 | grep -E '→|结束原因'

echo
echo "=== 日志里的读取记录 ==="
grep -o '"kind":"fs/read"[^}]*' "$WS/.ui/journal.jsonl" | head -2

echo
echo "=== 打包 ==="
"$BIN" lab pack --workspace "$WS" --name lin --out "$BUNDLE" >/dev/null
echo "inputs.jsonl:"
cat "$BUNDLE/inputs.jsonl"
echo
echo "report 里的「读过的输入」："
sed -n '/读过的输入/,/^$/p' "$BUNDLE/report.md"

echo "=== 校验（数据没变，应通过）==="
"$BIN" lab verify "$BUNDLE"; echo "退出码: $?"

echo
echo "=== 数据换版本后再校验（应失败）==="
printf 'x,y\n1,0\n2,1\n3,0\n' > "$WS/data.csv"
"$BIN" lab verify "$BUNDLE"; echo "退出码: $?"
