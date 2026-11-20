#!/usr/bin/env bash
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

git add -A
git -c user.name=ProMetaDev -c user.email=3144001558@qq.com commit -q -m 'fix(privacy): the web surface now redacts before it leaves the process (SSE, /snapshot, tool store); 0.2.2'
echo "提交: $(git rev-parse --short HEAD)"
git show --stat --oneline HEAD | tail -10
git push origin main 2>&1 | tail -1
