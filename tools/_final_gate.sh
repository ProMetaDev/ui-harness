#!/usr/bin/env bash
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

# 把自己误提交的临时脚本删掉
git rm -q --cached tools/_commit_gates.sh 2>/dev/null || true
rm -f tools/_commit_gates.sh
git add -A
git -c user.name=ProMetaDev -c user.email=3144001558@qq.com commit -q -m 'chore: drop scratch driver' 2>/dev/null || echo "（无需提交）"

echo "== 全量门禁 =="
bash tools/debug.sh 2>&1 | grep -E 'FAIL|== [0-9]'
echo
echo "工作区: $(git status --porcelain | wc -l)"
