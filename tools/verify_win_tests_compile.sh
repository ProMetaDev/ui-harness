#!/usr/bin/env bash
# 用 Windows target 做 cargo check --tests：只做类型检查、不链接，
# 能抓出"只在 Windows 上编译、Linux 上被 cfg 掉"的坏测试。
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"
bash tools/sync.sh >/dev/null
cd "$UI_BUILD_DIR" || exit 1

if ! rustup target list --installed 2>/dev/null | grep -q 'x86_64-pc-windows-gnu'; then
  echo "SKIP 没有装 x86_64-pc-windows-gnu target"
  exit 0
fi

CI_TARGET=x86_64-pc-windows-gnu cargo check --offline --tests --target x86_64-pc-windows-gnu 2>&1 | tail -8
STATUS=${PIPESTATUS[0]}
if [ "$STATUS" -ne 0 ]; then
  echo "FAIL Windows target 下测试代码编译不过（退出码 $STATUS）"
  exit 1
fi
echo "PASS Windows target 下测试代码可编译"
