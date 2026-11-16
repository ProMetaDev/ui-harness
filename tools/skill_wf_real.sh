#!/usr/bin/env bash
# 真模型：① 按名字加载 skill 并用它办事 ② 跑一个带审批闸门的两步工作流
set -uo pipefail
cd /mnt/x/dsh_ws/harness-v2
export CARGO_TARGET_DIR="$HOME/ui-target"
export UI_MODEL_CMD="bash tools/_node.sh tools/deepseek.mjs"
BIN="$CARGO_TARGET_DIR/release/ui"

cargo build --release --offline 2>&1 | tail -1
rm -rf demo/.ui demo/.loom

echo "=== 1. 目录里的 skill ==="
"$BIN" skills --workspace demo

echo
echo "=== 2. 真模型：让它自己决定要不要加载 skill ==="
"$BIN" run --task-file demo/task-skill.txt --workspace demo --yes --max-calls 6

echo
echo "=== 3. skill 加载留痕 ==="
"$BIN" replay --workspace demo --json | grep '"kind":"skill/load"' | head -2

echo
echo "=== 4. 工作流（第 2 步有闸门，--yes 自动批准）==="
"$BIN" workflow --file fixtures/workflow-review.json --workspace demo --yes --max-calls 8

echo
echo "=== 5. 工作流事件 ==="
"$BIN" replay --workspace demo --json | grep -E '"kind":"workflow/(start|step|end)"' \
  | sed -e 's/.*"kind":"\([^"]*\)".*/  \1/'
