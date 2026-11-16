#!/usr/bin/env bash
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

echo "== 1. 历史上有没有提交过 secrets/ 或 out/ =="
git log --all --oneline -- secrets/ out/ dist/ 2>/dev/null | head -10 || true
echo "（空 = 从未提交过）"

echo
echo "== 2. 全历史扫描密钥形状 =="
hits=$(git log -p --all 2>/dev/null | grep -E 'sk-[A-Za-z0-9_-]{20,}|AKIA[0-9A-Z]{16}|BEGIN [A-Z ]*PRIVATE KEY' | head -5)
if [ -z "$hits" ]; then echo "（干净）"; else echo "$hits"; fi

echo
echo "== 3. 提交者邮箱分布 =="
git log --all --format='%ae' | sort | uniq -c | sort -rn | head -5

echo
echo "== 4. 仓库里出现的本机路径/用户名（前 10 处）=="
grep -rIn --exclude-dir=.git --exclude-dir=target --exclude-dir=dist -E 'C:\\\\Users\\\\[0-9]+|/mnt/x/dsh_ws|31440' . 2>/dev/null | head -10 || echo "（没有）"

echo
echo "== 5. .gitignore 第一条是什么时候进来的 =="
git log --oneline -1 -- .gitignore
git show "$(git rev-list --max-parents=0 HEAD)":.gitignore 2>/dev/null | head -8 || echo "（首个提交没有 .gitignore）"
