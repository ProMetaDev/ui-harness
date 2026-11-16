#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# 量两件事：源码在 /mnt（9p）vs 在 WSL ext4，干净构建各要多久。
set -uo pipefail
cd "$UI_ROOT"
export CARGO_TARGET_DIR="$UI_TARGET_DIR"
echo "== A. 源码在 /mnt/x（9p），target 在 WSL ext4 =="
rm -rf "$CARGO_TARGET_DIR"
/usr/bin/time -f "  干净构建: %e 秒" cargo build --release --offline 2>&1 | tail -2

echo
echo "== B. 源码也在 WSL ext4（~/ui-src） =="
rm -rf "$UI_BUILD_DIR"-src"
mkdir -p "$UI_BUILD_DIR"-src"
cp -a Cargo.toml ui-core ui-cli "$UI_BUILD_DIR"-src"/
cd "$UI_BUILD_DIR"-src"
rm -rf "$CARGO_TARGET_DIR"
/usr/bin/time -f "  干净构建: %e 秒" cargo build --release --offline 2>&1 | tail -2

echo
echo "== C. 增量构建（改一行） =="
touch ui-core/src/main.rs 2>/dev/null || touch ui-core/src/lib.rs
/usr/bin/time -f "  ext4 增量: %e 秒" cargo build --release --offline 2>&1 | tail -1
cd "$UI_ROOT"
touch ui-core/src/lib.rs
/usr/bin/time -f "  /mnt 增量: %e 秒" cargo build --release --offline 2>&1 | tail -1
