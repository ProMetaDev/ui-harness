#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# 密钥卫生（D1/D3）：密钥类环境变量不能传给 shell 子进程，普通变量要保留。
set -uo pipefail
cd "$UI_ROOT"
BIN="$UI_BIN"
WS=/tmp/ui-env
rm -rf "$WS"; mkdir -p "$WS"

cat > "$WS/sc.json" <<'EOF'
{"model":"scripted","steps":[
  {"content":"看环境","toolCalls":[{"name":"shell","args":{"command":"echo S=${MY_API_KEY:-none} P=${MY_PLAIN:-none} X=$([ -n \"$PATH\" ] && echo yes)"}}]},
  {"content":"完"}]}
EOF

export MY_API_KEY=super-secret-value
export MY_PLAIN=hello

echo "=== 默认（应清掉密钥、保留普通变量）==="
out=$("$BIN" run --task t --mock "$WS/sc.json" --workspace "$WS" --yes 2>&1)
echo "$out" | grep -E '→'
echo "$out" | grep -q 'S=none' && echo "PASS 密钥没进子进程" || echo "FAIL 密钥泄漏了"
echo "$out" | grep -q 'P=hello' && echo "PASS 普通变量保留" || echo "FAIL 普通变量被误删"
echo "$out" | grep -q 'X=yes' && echo "PASS PATH 保留（命令能跑）" || echo "FAIL PATH 丢了"
