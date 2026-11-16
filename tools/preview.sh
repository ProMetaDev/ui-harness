#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# 渲染两帧（纯文本，便于贴进文档）+ 验证 --ansi 真的带颜色码。
set -uo pipefail
cd "$UI_ROOT"
export CARGO_TARGET_DIR="$UI_TARGET_DIR"
BIN="$CARGO_TARGET_DIR/release/ui"

cargo build --release --offline 2>&1 | tail -1

echo "=== transcript (84x26, no overlay) ==="
"$BIN" preview --cols 84 --rows 26 --no-approval

echo
echo "=== approval (64x16) ==="
"$BIN" preview --cols 64 --rows 16

echo
echo "=== ansi check ==="
"$BIN" preview --cols 84 --rows 8 --ansi | grep -c $'\x1b\[38;5;' || true
echo "distinct colours used:"
"$BIN" preview --cols 84 --rows 8 --ansi | grep -o $'\x1b\[[0-9;]*m' | sort -u | tr '\n' ' '
echo
