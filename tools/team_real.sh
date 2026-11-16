#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# 真模型协作模式：给一个真实的小项目补「测试 + 文档」，拆成 2 个子代理并行做。
set -uo pipefail
cd "$UI_ROOT"
BIN="$UI_BIN"
WS=/tmp/team-real
rm -rf "$WS"; mkdir -p "$WS"

cat > "$WS/math.js" <<'EOF'
export function add(a, b) {
  return a + b;
}

export function mul(a, b) {
  return a * b;
}
EOF

cat > "$WS/README.md" <<'EOF'
# math

两个纯函数：add / mul。

目前没有测试，也没有用法说明。
EOF

NODE_BIN=$(command -v node || echo /home/fairy/.nvm/versions/node/v22.23.3/bin/node)
export UI_MODEL_CMD="$NODE_BIN "$UI_ROOT"/tools/deepseek.mjs"

start=$(date +%s)
echo "=== 协作模式（真模型，2 个子代理）==="
"$BIN" team \
  --task "给 /tmp/team-real 这个 JS 小项目补上质量：需要（1）一份把 add/mul 覆盖到的单元测试，（2）README 补上安装与用法示例。保持现有函数行为不变。" \
  --agents 2 --workspace "$WS" --yes --max-calls 40 \
  2>&1 | tail -40
code=$?
echo "退出码 $code · 用时 $(( $(date +%s) - start ))s"

echo
echo "=== 工作区文件 ==="
ls -la "$WS" | tail -8

echo
echo "=== 子代理的隔离副本（delegate 记录）==="
python3 - "$WS/.ui/journal.jsonl" <<'PY'
import json, sys
for line in open(sys.argv[1], encoding="utf-8"):
    ev = json.loads(line)
    k = ev.get("kind", "")
    if k in ("delegate/start", "delegate/end"):
        keep = {x: ev[x] for x in ("agent", "workdir", "ok", "changed", "reason") if x in ev}
        print(k, json.dumps(keep, ensure_ascii=False)[:300])
PY
