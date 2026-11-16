#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# A + C 端到端：跑一轮 → 打包 → 校验 → 篡改 → 校验必须失败。
set -uo pipefail
cd "$UI_ROOT"
BIN="$UI_BIN"
WS=/tmp/ui-lab
BUNDLE=/tmp/ui-lab-bundle
rm -rf "$WS" "$BUNDLE"; mkdir -p "$WS"

cat > "$WS/train.py" <<'PY'
import json, sys
seed = int(sys.argv[1]) if len(sys.argv) > 1 else 0
acc = 0.80 + seed * 0.01
json.dump({"seed": seed, "accuracy": acc}, open(f"metrics-{seed}.json", "w"))
print(f"seed={seed} accuracy={acc}")
PY

cat > "$WS/sc.json" <<'EOF'
{"model":"scripted","steps":[
  {"content":"跑实验","toolCalls":[{"name":"shell","args":{"command":"python3 train.py 1"}}]},
  {"content":"存结论","toolCalls":[{"name":"fs_write","args":{"path":"metrics-1.json","content":"{\"seed\":1,\"accuracy\":0.81}"}}]},
  {"content":"好了"}]}
EOF

echo "=== 1. 跑一轮 ==="
"$BIN" run --task "跑个实验" --mock "$WS/sc.json" --workspace "$WS" --mode standard --yes 2>&1 | grep -E '→|结束原因'

echo
echo "=== 2. 打包 ==="
"$BIN" lab pack --workspace "$WS" --name exp1 --out "$BUNDLE"
echo
echo "--- report.md ---"
head -22 "$BUNDLE/report.md"

echo
echo "=== 3. 校验（应通过）==="
"$BIN" lab verify "$BUNDLE"; echo "退出码: $?"

echo
echo "=== 4. 篡改产物后再校验（应失败）==="
echo '{"seed":1,"accuracy":0.99}' > "$BUNDLE/artifacts/metrics-1.json"
"$BIN" lab verify "$BUNDLE"; echo "退出码: $?"
