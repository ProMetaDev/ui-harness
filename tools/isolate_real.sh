#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# 真模型 + 隔离并行：两个子代理各写各的文档，父工作区不被污染，再显式 apply 回写。
set -uo pipefail
cd "$UI_ROOT"
export UI_MODEL_CMD="bash tools/_node.sh tools/deepseek.mjs"
BIN="$UI_BIN"
bash tools/dev.sh build --release 2>&1 | tail -1
rm -rf demo/.ui demo/SECURITY.md demo/USABILITY.md

echo "=== 跑隔离工作流 ==="
"$BIN" workflow --file fixtures/workflow-write.json --workspace demo --isolate --yes --max-calls 8

echo
echo "=== 父工作区有没有被污染 ==="
ls demo/SECURITY.md demo/USABILITY.md 2>&1 | sed 's/^/  /'

echo
echo "=== 子代理记账（id / ok / 改动文件）==="
"$BIN" replay --workspace demo --json | grep 'delegate/end' | head -2 \
  | sed -e 's/.*"id":"\([^"]*\)","ok":\([a-z]*\).*/  \1 ok=\2/'

echo
echo "=== 计划模式回写（不写盘）==="
ID=$(grep -o '"kind":"delegate/end","id":"[^"]*"' demo/.ui/journal.jsonl | head -1 | sed 's/.*"id":"//;s/"//')
echo "  id=$ID"
"$BIN" apply --id "$ID" --workspace demo

echo
echo "=== 真回写 ==="
"$BIN" apply --id "$ID" --workspace demo --yes
ls -l demo/*.md 2>/dev/null | sed 's/^/  /'
