#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# 你自己在 WSL 里跑这个：构建 → 重置 demo → 进全屏 TUI。
# 场景会先请求执行一条 shell 命令，所以你会看到审批浮层：按 y 放行，按 q 退出。
set -euo pipefail
cd "$UI_ROOT"
export CARGO_TARGET_DIR="$UI_TARGET_DIR"
cargo build --release --offline 2>&1 | tail -1

cat > demo/greet.js <<'EOF'
export function greet(name) {
  return `hi ${name}`;
}

console.log(greet("loom"));
console.log(greet(""));
EOF
rm -rf demo/.ui

exec "$CARGO_TARGET_DIR/release/ui" tui \
  --mock fixtures/scenarios/tui-shell.json \
  --workspace demo
