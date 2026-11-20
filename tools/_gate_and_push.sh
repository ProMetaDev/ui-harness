#!/usr/bin/env bash
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"
git add -A
git -c user.name=ProMetaDev -c user.email=3144001558@qq.com commit -q -m 'test(web): allowlist the leak-canary fixture so the secret gate stays honest'
bash tools/debug.sh 2>&1 | grep -E 'FAIL|== [0-9]'
echo
git push origin main 2>&1 | tail -1
echo "工作区: $(git status --porcelain | wc -l)"
