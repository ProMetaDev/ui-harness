#!/usr/bin/env bash
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

# 清掉临时脚本
rm -f tools/_prepush.sh tools/_sshcheck.sh tools/_check_agents.sh

git add -A
git -c user.name=ProMetaDev -c user.email=3144001558@qq.com commit -q -m 'chore: clean temp scripts before first push' 2>/dev/null || true

echo "== 配置远端 =="
git remote remove origin 2>/dev/null || true
git remote add origin git@github.com:ProMetaDev/ui-harness.git
git remote -v

echo
echo "== 探测远端是否为空 =="
if git ls-remote --heads origin >/tmp/heads.txt 2>/tmp/ls.err; then
  n=$(wc -l < /tmp/heads.txt)
  if [ "$n" -eq 0 ]; then
    echo "  远端是空仓库（可以直接 push）"
  else
    echo "  远端已有 $n 个分支："
    cat /tmp/heads.txt
  fi
else
  echo "  ls-remote 失败：$(head -2 /tmp/ls.err)"
fi
