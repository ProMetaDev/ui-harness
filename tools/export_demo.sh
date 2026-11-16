#!/usr/bin/env bash
# 生成一份真实轨迹并导出单文件查看器，再用 node 校验内嵌数据能解析。
set -uo pipefail
cd /mnt/x/dsh_ws/harness-v2
export CARGO_TARGET_DIR="$HOME/ui-target"
BIN="$CARGO_TARGET_DIR/release/ui"

cargo build --release --offline 2>&1 | tail -1

cat > demo/greet.js <<'EOF'
export function greet(name) {
  return `hi ${name}`;
}

console.log(greet("loom"));
console.log(greet(""));
EOF
rm -rf demo/.ui

"$BIN" run --task-file demo/task.txt --mock fixtures/scenarios/fix-bug.json \
  --workspace demo --yes >/dev/null

"$BIN" export --workspace demo --html out/trace.html \
  --max-tokens 200000 --max-calls 60

echo "文件大小 $(wc -c < out/trace.html) 字节"
echo "校验：node tools/check_viewer.mjs out/trace.html"
