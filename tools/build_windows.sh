#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# 用本地解压的 mingw 链接 Windows 版 ui.exe
set -uo pipefail
export PATH="$HOME/.cargo/bin:$HOME/mingw64/root/usr/bin:$PATH"
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER="$HOME/mingw64/root/usr/bin/x86_64-w64-mingw32-gcc-posix"

echo "== 链接器 =="
ls -l "$CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER" 2>/dev/null || echo "缺链接器"

cd "$UI_BUILD_DIR"" || exit 1
echo "== 构建 ui.exe =="
cargo build --offline --release --target x86_64-pc-windows-gnu -p ui-cli 2>&1 | tail -12
echo
ls -lh "$UI_TARGET_DIR"/x86_64-pc-windows-gnu/release/ui.exe" 2>/dev/null || echo "没有产物"
