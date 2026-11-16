#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# 真模型 demo：DeepSeek 跑一轮真实修改，然后自检 + 导出轨迹。
# 前置：secrets/providers.toml 的 [chat].api_key（或环境变量 DEEPSEEK_API_KEY）。
set -uo pipefail
cd "$UI_ROOT"
export CARGO_TARGET_DIR="$UI_TARGET_DIR"
BIN="$CARGO_TARGET_DIR/release/ui"
export UI_MODEL_CMD="bash tools/_node.sh tools/deepseek.mjs"

cargo build --release --offline 2>&1 | tail -1

cat > demo/greet.js <<'EOF'
export function greet(name) {
  return `hi ${name}`;
}

console.log(greet("loom"));
console.log(greet(""));
EOF
rm -rf demo/.ui

echo "=== 模型 ==="
bash tools/_node.sh tools/probe_chat.mjs

echo
echo "=== 一轮真实任务 ==="
time "$BIN" run --task-file demo/task.txt --workspace demo --yes

echo
echo "=== 结果文件 ==="
cat demo/greet.js

echo
echo "=== 日志自检 ==="
"$BIN" verify --workspace demo

echo
echo "=== 导出轨迹 ==="
"$BIN" export --workspace demo --html out/trace.html
