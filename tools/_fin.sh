#!/usr/bin/env bash
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"
git add -A
git -c user.name=ProMetaDev -c user.email=3144001558@qq.com commit -q -m 'build: watch git refs so --version reflects HEAD' || true
bash tools/dev.sh build --release 2>&1 | grep -E '^error|Finished' | head -2
echo "--- 版本（应无 -dirty）---"
"$UI_BIN" --version
echo "--- 体积 ---"
ls -lh "$UI_BIN" | awk '{print $5}'
