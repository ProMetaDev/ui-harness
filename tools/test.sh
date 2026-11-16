#!/usr/bin/env bash
# 构建 + 测试的固定入口（快路径：/mnt → ext4 副本再编译）。
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"

echo "== sync + build =="
bash "$HERE/sync.sh"

cd "${UI_BUILD_DIR:-$HOME/ui}"
cargo test --offline 2>&1 | grep -E "^error|error\[|^warning|test result|panicked" || true
