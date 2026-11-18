#!/usr/bin/env bash
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"
git status --porcelain
git add -A 2>/dev/null
git -c user.name=ProMetaDev -c user.email=3144001558@qq.com commit -q -m 'chore: 0.2.1 clean' 2>/dev/null || echo "（无需提交）"
git push origin main 2>&1 | tail -1
bash tools/ct.sh build --release >/dev/null 2>&1
"$UI_BIN" --version
echo "工作区: $(git status --porcelain | wc -l)"
