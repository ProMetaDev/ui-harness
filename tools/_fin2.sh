#!/usr/bin/env bash
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"
git add -A
git -c user.name=ProMetaDev -c user.email=3144001558@qq.com commit -q -m 'chore: drop scratch scripts' 2>/dev/null || true
git push origin main 2>&1 | tail -1
bash tools/dev.sh build --release 2>&1 | tail -1
"$UI_BIN" --version
echo "工作区: $(git status --porcelain | wc -l) 个改动"
echo "标签: $(git tag | tr '\n' ' ')"
