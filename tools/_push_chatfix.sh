#!/usr/bin/env bash
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"
git add -A
git -c user.name=ProMetaDev -c user.email=3144001558@qq.com commit -q -m 'test(web): queue case needs the server taskId; CI must not silently skip the chat check'
git push origin main 2>&1 | tail -1
echo "HEAD: $(git rev-parse --short HEAD)"
git status --porcelain | wc -l
