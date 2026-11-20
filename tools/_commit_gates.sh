#!/usr/bin/env bash
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

git add -A
git -c user.name=ProMetaDev -c user.email=3144001558@qq.com commit -q -m 'fix(gates): docs gate was red on its own comment and three stale refs; add web-leak baseline'
echo "提交: $(git rev-parse --short HEAD)"
git show --stat --oneline HEAD | tail -8
echo
echo "== 单独跑文档门禁 =="
bash tools/verify_docs.sh 2>&1 | tail -3
