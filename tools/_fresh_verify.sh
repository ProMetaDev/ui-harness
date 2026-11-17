#!/usr/bin/env bash
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

echo "== debug.sh 开头 =="
sed -n '1,12p' tools/debug.sh

echo
echo "== 全新空工作区跑 ui verify =="
rm -rf /tmp/ui-fresh-verify
mkdir -p /tmp/ui-fresh-verify
"$UI_BIN" verify --workspace /tmp/ui-fresh-verify; echo "退出码: $?"
echo
echo "== 有日志的工作区 =="
rm -rf /tmp/ui-fresh-verify2 && mkdir -p /tmp/ui-fresh-verify2
cat > /tmp/ui-fresh-verify2/sc.json <<'JSON'
{"model":"scripted","steps":[{"content":"x","toolCalls":[{"name":"fs_write","args":{"path":"a.txt","content":"x"}}]},{"content":"done"}]}
JSON
"$UI_BIN" run --task t --mock /tmp/ui-fresh-verify2/sc.json --workspace /tmp/ui-fresh-verify2 --yes >/dev/null 2>&1
"$UI_BIN" verify --workspace /tmp/ui-fresh-verify2; echo "退出码: $?"
