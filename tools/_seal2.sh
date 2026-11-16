#!/usr/bin/env bash
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"
rm -f tools/_push_obs.sh
git add -A
git -c user.name=ProMetaDev -c user.email=3144001558@qq.com commit -q -m 'build: rebuild after README update' 2>/dev/null || true
bash tools/dev.sh build --release 2>&1 | tail -1
git push origin main 2>&1 | tail -1
"$UI_BIN" --version
echo "工作区改动: $(git status --porcelain | wc -l)"
