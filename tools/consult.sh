#!/usr/bin/env bash
# 把简报发给强模型做评审，结果写到 out/consult-<时间>.md
#
# 用法：
#   1. 填 secrets/astra.json（base_url / api_key / model）
#   2. bash tools/consult.sh
#
# 可选：BRIEF=docs/other.md bash tools/consult.sh
set -uo pipefail
cd /mnt/x/dsh_ws/harness-v2

BRIEF=${BRIEF:-docs/consult-brief.md}
OUTDIR=${OUTDIR:-out}
STAMP=$(date +%Y%m%d-%H%M%S)
OUT="$OUTDIR/consult-$STAMP.md"
mkdir -p "$OUTDIR"

if [ ! -f secrets/astra.json ]; then
  echo "没有 secrets/astra.json：先填 base_url / api_key / model"
  exit 1
fi

NODE_BIN=$(command -v node || echo /home/fairy/.nvm/versions/node/v22.23.3/bin/node)
if [ ! -x "$(command -v $NODE_BIN)" ] && [ ! -x "$NODE_BIN" ]; then
  echo "找不到 node"
  exit 1
fi

echo "简报: $BRIEF ($(wc -l < "$BRIEF") 行)"
echo "输出: $OUT"

# 把简报包成适配器要的输入
PAYLOAD=$(python3 - "$BRIEF" <<'PY'
import json, sys
text = open(sys.argv[1], encoding="utf-8").read()
print(json.dumps({"prompt": text, "snapshotId": "consult"}))
PY
)

START=$(date +%s)
printf '%s' "$PAYLOAD" | "$NODE_BIN" tools/chat_compat.mjs > "$OUTDIR/.consult-raw.json" 2>"$OUTDIR/.consult-err.txt"
CODE=$?
SECS=$(( $(date +%s) - START ))

if [ $CODE -ne 0 ]; then
  echo "调用失败（退出码 $CODE，${SECS}s）:"
  cat "$OUTDIR/.consult-err.txt"
  exit $CODE
fi

python3 - "$OUTDIR/.consult-raw.json" "$OUT" "$SECS" <<'PY'
import json, sys
raw = open(sys.argv[1], encoding="utf-8").read().strip()
doc = json.loads(raw)
content = doc.get("content") or "(空回复)"
tokens = doc.get("usageTokens") or 0
header = f"<!-- 模型输出 · {sys.argv[3]}s · {tokens} tok · 生成于 {sys.argv[2]} -->\n\n"
open(sys.argv[2], "w", encoding="utf-8").write(header + content + "\n")
print(f"完成：{len(content)} 字符 · {tokens} tok")
PY
rm -f "$OUTDIR/.consult-raw.json" "$OUTDIR/.consult-err.txt"
echo "结果: $OUT"
