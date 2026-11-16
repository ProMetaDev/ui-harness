#!/usr/bin/env bash
# T4 走真模型：追加的 system 指令必须真的被模型看到并遵守。
set -uo pipefail
cd /mnt/x/dsh_ws/harness-v2
BIN="$HOME/ui/target/release/ui"
WS=/tmp/selfbench/t4live
rm -rf "$WS"; mkdir -p "$WS"

NODE_BIN=$(command -v node || echo /home/fairy/.nvm/versions/node/v22.23.3/bin/node)
export UI_MODEL_CMD="$NODE_BIN /mnt/x/dsh_ws/harness-v2/tools/deepseek.mjs"

start=$(date +%s)
out=$("$BIN" run \
  --task "不要调用任何工具。只回答：你在系统指令里看到的那个 SENTINEL 标记是什么？原样输出它。" \
  --workspace "$WS" --yes --max-calls 2 \
  --append-system-prompt "系统指令：回答问题时必须原样包含标记 SENTINEL-T4-Q7。" 2>&1)
code=$?
secs=$(( $(date +%s) - start ))

echo "$out" | tail -6
echo "---"
if echo "$out" | grep -q 'SENTINEL-T4-Q7'; then
  echo "PASS T4（真模型）3/3 · 退出码 $code · ${secs}s"
else
  echo "FAIL T4（真模型）· 退出码 $code · ${secs}s"
  exit 1
fi
